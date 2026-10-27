//! Phase-0 go/no-go spike: how fast can headless Chrome screencast into this crate?
//!
//! Loads local pages in real Chrome (no display needed) and prints, per window size:
//! sustained frames/s on a scrolling page and on a 60 fps animation page, decode time (p50/p95
//! of the JPEG decode), Chrome + our own CPU in cores, frames on an idle static page, and the
//! input -> pixel latency (a mousedown flips a colour in a rAF callback; the time is measured
//! to the first frame that shows the new colour).
//!
//! ```text
//! cargo run -p hivegrid-web --example spike --release -- [options]
//!   --sizes 1000x800,1920x1080   window sizes in physical pixels
//!   --dpi 96                     96 = scale 1.0
//!   --secs 8                     length of the fps/CPU scenarios
//!   --quality 80                 screencast JPEG quality (HIVEGRID_WEB_QUALITY)
//!   --flag <chrome flag>         extra Chrome flag, repeatable (e.g. --flag=--use-angle=vulkan)
//!   --scenarios scroll,anim,idle,latency,start
//!   --trials 80                  latency trials
//! ```

use hivegrid_web::{
    take_decode_times, CdpEngine, Frame, ViewSize, WebConfig, WebEngine, WebEvent, WebMouse,
    WebMouseKind, WebView,
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCROLL_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>scroll</title>
<style>
 body{margin:0;font:15px/1.5 sans-serif;background:#fafafa;color:#222}
 p{margin:0 0 .8em;padding:.6em 1em;border-radius:8px;box-shadow:0 1px 4px rgba(0,0,0,.25)}
 p:nth-child(3n){background:linear-gradient(90deg,#fde,#def)} p:nth-child(3n+1){background:#fff}
 p:nth-child(3n+2){background:linear-gradient(#efe,#ffe)}
 b{color:#a30}
</style><div id=c></div>
<script>
 const words='lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua'.split(' ');
 let html='';
 for(let i=0;i<500;i++){let s='';for(let j=0;j<70;j++){const w=words[(i*7+j*3)%words.length];s+=(j%11==0?'<b>'+w+'</b>':w)+' '}html+='<p>'+i+'. '+s+'</p>'}
 c.innerHTML=html;
 let y=0,dir=1,n=0,t0=performance.now();
 function step(){ y+=dir*14; const m=document.body.scrollHeight-innerHeight; if(y>=m){y=m;dir=-1} if(y<=0){y=0;dir=1}
   scrollTo(0,y); n++; const t=performance.now(); if(t-t0>=1000){document.title='raf='+Math.round(n*1000/(t-t0));n=0;t0=t}
   requestAnimationFrame(step)}
 requestAnimationFrame(step);
</script>"#;

const ANIM_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>anim</title>
<style>
 body{margin:0;overflow:hidden;background:#101820;color:#cde;font:16px sans-serif}
 .b{position:absolute;width:90px;height:90px;border-radius:12px;opacity:.85;will-change:transform}
 #txt{position:absolute;left:20px;top:20px;white-space:pre;opacity:.6}
</style><div id=txt></div><div id=w></div>
<script>
 const w=document.getElementById('w'),B=[];
 for(let i=0;i<60;i++){const d=document.createElement('div');d.className='b';d.style.background='hsl('+(i*6)+',80%,55%)';w.appendChild(d);B.push(d)}
 document.getElementById('txt').textContent=Array.from({length:30},(_, i)=>'line '+i+' of animated page text, to give the encoder something to chew').join('\n');
 let n=0,t0=performance.now();
 function step(t){ for(let i=0;i<B.length;i++){const a=t/900+i*.31; B[i].style.transform='translate('+(innerWidth/2+Math.cos(a)*innerWidth*.35-45)+'px,'+(innerHeight/2+Math.sin(a*1.3)*innerHeight*.35-45)+'px) rotate('+(t/10+i*9)+'deg)'}
   n++; if(t-t0>=1000){document.title='raf='+Math.round(n*1000/(t-t0));n=0;t0=t}
   requestAnimationFrame(step)}
 requestAnimationFrame(step);
</script>"#;

const IDLE_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>idle</title>
<body style="font:20px sans-serif;background:#fff"><h1>Static page</h1><p>Nothing moves here.</p>"#;

/// A text page with one small compositor-only CSS animation: the usual "idle but animating"
/// page (a loading spinner).
const SPINNER_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>spinner</title>
<style>body{font:20px sans-serif;margin:20px} #s{width:40px;height:40px;background:#08f;border-radius:6px;
 animation:r 1.2s linear infinite} @keyframes r{to{transform:rotate(360deg)}}</style>
<h1>Loading...</h1><p>Some static text around a spinner.</p><div id=s></div>"#;

/// A text page with a blinking caret: about two frames per second.
const BLINK_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>blink</title>
<body style="font:20px sans-serif;margin:20px"><h1>Editor</h1><span id=c style="border-left:2px solid #000">&nbsp;</span>
<script>let on=true;setInterval(()=>{on=!on;c.style.borderLeftColor=on?'#000':'#fff'},530)</script>"#;

/// mousedown flips the colour inside a requestAnimationFrame callback.
const LATENCY_PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>latency</title>
<body style="margin:0;background:#000"><script>
 let flag=false,white=false;
 addEventListener('mousedown',()=>{flag=true});
 function step(){ if(flag){flag=false;white=!white;document.body.style.background=white?'#fff':'#000'} requestAnimationFrame(step)}
 requestAnimationFrame(step);
</script>"#;

struct Collector {
    view: Mutex<Option<Arc<dyn WebView>>>,
    frames: Mutex<Vec<(Instant, u64, [u8; 3])>>,
}

impl Collector {
    fn new() -> Arc<Self> {
        Arc::new(Collector {
            view: Mutex::new(None),
            frames: Mutex::new(Vec::new()),
        })
    }

    fn notify(self: &Arc<Self>) -> Box<dyn Fn(WebEvent) + Send + Sync> {
        let me = self.clone();
        Box::new(move |ev| {
            if ev == WebEvent::Frame {
                let now = Instant::now();
                let view = me.view.lock().clone();
                if let Some(f) = view.and_then(|v| v.take_frame()) {
                    me.frames.lock().push((now, f.seq, centre(&f)));
                }
            }
        })
    }

    fn attach(&self, v: Arc<dyn WebView>) {
        *self.view.lock() = Some(v);
    }

    fn clear(&self) {
        self.frames.lock().clear();
    }

    fn len(&self) -> usize {
        self.frames.lock().len()
    }
}

fn centre(f: &Frame) -> [u8; 3] {
    let i = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
    [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2]]
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

/// CPU seconds (user + system) of every process in the browser's process group.
fn group_cpu_seconds(pgid: u32) -> f64 {
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let mut ticks = 0u64;
    let Ok(dir) = std::fs::read_dir("/proc") else { return 0.0 };
    for e in dir.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        // rest[0] is field 3 (state): pgrp is field 5 -> index 2, utime 14 -> 11, stime 15 -> 12.
        if f.len() > 12 && f[2].parse::<u32>().ok() == Some(pgid) {
            ticks += f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
        }
    }
    ticks as f64 / tck
}

fn own_cpu_seconds() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

struct Args {
    sizes: Vec<(u32, u32)>,
    dpi: u32,
    secs: f64,
    flags: Vec<String>,
    scenarios: Vec<String>,
    trials: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        sizes: vec![(1000, 800), (1920, 1080)],
        dpi: 96,
        secs: 8.0,
        flags: vec![],
        scenarios: ["start", "scroll", "anim", "spinner", "blink", "idle", "latency"]
            .map(String::from)
            .to_vec(),
        trials: 80,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        match k.as_str() {
            "--sizes" => {
                a.sizes = val()
                    .split(',')
                    .map(|s| {
                        let (w, h) = s.split_once('x').expect("WxH");
                        (w.parse().unwrap(), h.parse().unwrap())
                    })
                    .collect()
            }
            "--dpi" => a.dpi = val().parse().unwrap(),
            "--secs" => a.secs = val().parse().unwrap(),
            "--quality" => std::env::set_var("HIVEGRID_WEB_QUALITY", val()),
            "--flag" => a.flags.push(val()),
            f if f.starts_with("--flag=") => a.flags.push(f["--flag=".len()..].to_string()),
            "--scenarios" => a.scenarios = val().split(',').map(String::from).collect(),
            "--trials" => a.trials = val().parse().unwrap(),
            other => panic!("unknown option {other}"),
        }
    }
    a
}

fn wait_for(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

struct Rig {
    engine: CdpEngine,
    cfg: WebConfig,
    dpi: u32,
    dir: tempfile::TempDir,
    _anchor: Arc<dyn WebView>,
}

impl Rig {
    fn page(&self, name: &str, html: &str) -> String {
        let p = self.dir.path().join(format!("{name}.html"));
        std::fs::write(&p, html).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn open(&self, url: &str, w: u32, h: u32) -> (Arc<dyn WebView>, Arc<Collector>) {
        let col = Collector::new();
        let v = self
            .engine
            .create_view(
                url,
                &self.cfg,
                ViewSize { pixel_width: w, pixel_height: h, dpi: self.dpi },
                col.notify(),
            )
            .unwrap();
        col.attach(v.clone());
        (v, col)
    }

    fn pgid(&self) -> u32 {
        self.engine.browser_pids().first().copied().expect("browser running")
    }
}

struct Run {
    frames: usize,
    fps: f64,
    interval_p50: f64,
    interval_p95: f64,
    decode: Vec<f64>,
    chrome_cores: f64,
    own_cores: f64,
    raf: String,
    dims: (u32, u32),
}

fn measure(rig: &Rig, url: &str, w: u32, h: u32, secs: f64) -> Run {
    let (v, col) = rig.open(url, w, h);
    assert!(
        wait_for(Duration::from_secs(20), || col.len() > 0),
        "no frame from {url}: state {:?}",
        v.state()
    );
    // Let the page settle (layout, first scroll, JIT) before the window opens.
    std::thread::sleep(Duration::from_millis(1500));
    col.clear();
    let _ = take_decode_times();
    let pg = rig.pgid();
    let (c0, o0, t0) = (group_cpu_seconds(pg), own_cpu_seconds(), Instant::now());
    std::thread::sleep(Duration::from_secs_f64(secs));
    let (c1, o1, el) = (group_cpu_seconds(pg), own_cpu_seconds(), t0.elapsed().as_secs_f64());
    let frames = col.frames.lock().clone();
    let decode: Vec<f64> = sorted(take_decode_times().iter().map(|d| d.as_secs_f64() * 1e3).collect());
    let intervals = sorted(
        frames.windows(2).map(|p| p[1].0.duration_since(p[0].0).as_secs_f64() * 1e3).collect(),
    );
    let dims = v.take_frame().map(|f| (f.width, f.height)).unwrap_or((0, 0));
    let raf = v.info().title;
    v.close();
    Run {
        frames: frames.len(),
        fps: frames.len() as f64 / el,
        interval_p50: percentile(&intervals, 0.5),
        interval_p95: percentile(&intervals, 0.95),
        decode,
        chrome_cores: (c1 - c0) / el,
        own_cores: (o1 - o0) / el,
        raf,
        dims,
    }
}

fn report(label: &str, r: &Run) {
    println!(
        "{label}: {:.1} fps ({} frames, frame {}x{}), interval p50 {:.1} ms p95 {:.1} ms, decode p50 {:.2} ms p95 {:.2} ms max {:.2} ms, \
         CPU chrome {:.2} + own {:.2} = {:.2} cores, page {}",
        r.fps,
        r.frames,
        r.dims.0,
        r.dims.1,
        r.interval_p50,
        r.interval_p95,
        percentile(&r.decode, 0.5),
        percentile(&r.decode, 0.95),
        r.decode.last().copied().unwrap_or(f64::NAN),
        r.chrome_cores,
        r.own_cores,
        r.chrome_cores + r.own_cores,
        r.raf,
    );
}

fn latency(rig: &Rig, w: u32, h: u32, trials: usize) -> (Vec<f64>, Vec<f64>) {
    let url = rig.page("latency", LATENCY_PAGE);
    let (v, col) = rig.open(&url, w, h);
    assert!(wait_for(Duration::from_secs(20), || col.len() > 0), "no first frame");
    std::thread::sleep(Duration::from_millis(800));
    // Warm up: a few flips so the first-click path is not measured.
    let mut white = false;
    let mut next = Vec::new();
    let mut pixel = Vec::new();
    let mut rng = 0x2545F4914F6CDD1Du64;
    let click = |down: bool, x: f32, y: f32| {
        v.mouse(WebMouse {
            kind: if down { WebMouseKind::Down } else { WebMouseKind::Up },
            x,
            y,
            button: 0,
            buttons: if down { 1 } else { 0 },
            mods: 0,
            click_count: 1,
            dx: 0.0,
            dy: 0.0,
        })
    };
    let (cx, cy) = (w as f32 / 2.0 / (rig.dpi as f32 / 96.0), h as f32 / 2.0 / (rig.dpi as f32 / 96.0));
    for i in 0..trials + 5 {
        // Quiet period, so the previous flip's frames have all arrived.
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(150 + rng % 100));
        let want_white = !white;
        col.clear();
        let t0 = Instant::now();
        click(true, cx, cy);
        let (mut t_next, mut t_pixel) = (None, None);
        let end = t0 + Duration::from_secs(3);
        while Instant::now() < end && t_pixel.is_none() {
            for (t, _, rgb) in col.frames.lock().iter() {
                t_next.get_or_insert(t.duration_since(t0));
                let lum = (rgb[0] as u32 + rgb[1] as u32 + rgb[2] as u32) / 3;
                if (lum > 128) == want_white {
                    t_pixel = Some(t.duration_since(t0));
                    break;
                }
            }
            std::thread::sleep(Duration::from_micros(300));
        }
        click(false, cx, cy);
        if let (Some(n), Some(p)) = (t_next, t_pixel) {
            white = want_white;
            if i >= 5 {
                next.push(n.as_secs_f64() * 1e3);
                pixel.push(p.as_secs_f64() * 1e3);
            }
        } else {
            eprintln!("trial {i}: no frame with the new colour within 3 s");
        }
    }
    v.close();
    (sorted(next), sorted(pixel))
}

/// Prints the crate's warnings (undecodable frames, failed starts) to stderr.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() {
    let level = match std::env::var("SPIKE_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Warn,
    };
    let _ = log::set_logger(&StderrLog).map(|()| log::set_max_level(level));
    let args = parse_args();
    let dir = tempfile::tempdir().unwrap();
    let cfg = WebConfig {
        chrome: None,
        profile: "spike".into(),
        private: false,
        data_root: dir.path().join("data"),
        extra_flags: args.flags.clone(),
    };
    let engine = CdpEngine::new();
    println!(
        "spike: sizes {:?} dpi {} secs {} quality {} flags {:?}",
        args.sizes,
        args.dpi,
        args.secs,
        std::env::var("HIVEGRID_WEB_QUALITY").unwrap_or_else(|_| "80 (default)".into()),
        args.flags
    );

    // The anchor keeps the browser alive between scenarios (and is the cold-start measurement).
    let want = |s: &str| args.scenarios.iter().any(|x| x == s);
    let t0 = Instant::now();
    let col = Collector::new();
    let anchor = engine
        .create_view(
            "about:blank",
            &cfg,
            ViewSize { pixel_width: 400, pixel_height: 300, dpi: args.dpi },
            col.notify(),
        )
        .unwrap();
    col.attach(anchor.clone());
    let got_first = wait_for(Duration::from_secs(40), || col.len() > 0 || matches!(anchor.state(), hivegrid_web::WebState::Crashed { .. }));
    if !got_first || matches!(anchor.state(), hivegrid_web::WebState::Crashed { .. }) {
        println!("FAILED to start Chrome: {:?}", anchor.state());
        return;
    }
    let cold = t0.elapsed();
    let rig = Rig { engine, cfg, dpi: args.dpi, dir, _anchor: anchor };
    if want("start") {
        let t1 = Instant::now();
        let (v, c) = rig.open("data:text/html,<body style=background:%23f0f>", 400, 300);
        let ok = wait_for(Duration::from_secs(20), || c.len() > 0);
        println!(
            "startup: cold browser -> first frame {:.0} ms; warm second view -> first frame {} ms",
            cold.as_secs_f64() * 1e3,
            if ok { format!("{:.0}", t1.elapsed().as_secs_f64() * 1e3) } else { "none".into() }
        );
        v.close();
    }

    for &(w, h) in &args.sizes {
        println!("--- {w}x{h} @ {} dpi ---", args.dpi);
        if want("scroll") {
            let url = rig.page("scroll", SCROLL_PAGE);
            report(&format!("scroll {w}x{h}"), &measure(&rig, &url, w, h, args.secs));
        }
        if want("anim") {
            let url = rig.page("anim", ANIM_PAGE);
            report(&format!("anim   {w}x{h}"), &measure(&rig, &url, w, h, args.secs));
        }
        if want("spinner") {
            let url = rig.page("spinner", SPINNER_PAGE);
            report(&format!("spinner {w}x{h}"), &measure(&rig, &url, w, h, args.secs));
        }
        if want("blink") {
            let url = rig.page("blink", BLINK_PAGE);
            report(&format!("blink  {w}x{h}"), &measure(&rig, &url, w, h, args.secs));
        }
        if want("idle") {
            let url = rig.page("idle", IDLE_PAGE);
            let r = measure(&rig, &url, w, h, 5.0);
            println!(
                "idle   {w}x{h}: {} frames in 5 s on a static page, CPU chrome {:.3} + own {:.3} cores",
                r.frames, r.chrome_cores, r.own_cores
            );
        }
        if want("latency") {
            let (next, pixel) = latency(&rig, w, h, args.trials);
            println!(
                "latency {w}x{h}: input -> next frame p50 {:.1} p95 {:.1} ms; input -> new pixel p50 {:.1} p95 {:.1} max {:.1} ms ({} trials)",
                percentile(&next, 0.5),
                percentile(&next, 0.95),
                percentile(&pixel, 0.5),
                percentile(&pixel, 0.95),
                pixel.last().copied().unwrap_or(f64::NAN),
                pixel.len()
            );
        }
    }
    let pids = rig.engine.browser_pids();
    println!("browser pids before exit: {pids:?}");
}
