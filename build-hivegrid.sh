#!/usr/bin/env bash
# Build the Hivegrid WezTerm fork without sudo and deploy it next to the system
# WezTerm: ~/.local/opt/hivegrid-wezterm plus the wrapper ~/.local/bin/hivegrid.
# Safe to re-run. WITH_MUX=1 also builds and deploys wezterm-mux-server;
# SKIP_BUILD=1 only redeploys the binaries already in target/release.
set -euo pipefail

SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SYSROOT=${SYSROOT:-$HOME/.local/sysroot}
LIBDIR=$SYSROOT/usr/lib/x86_64-linux-gnu
OPT=${OPT:-$HOME/.local/opt/hivegrid-wezterm}
WRAPPER=${WRAPPER:-$HOME/.local/bin/hivegrid}

packages=(-p wezterm-gui -p wezterm)
bins=(wezterm wezterm-gui)
if [ "${WITH_MUX:-0}" = 1 ]; then
  packages+=(-p wezterm-mux-server)
  bins+=(wezterm-mux-server)
fi

if [ "${SKIP_BUILD:-0}" != 1 ]; then
  (
    cd "$SRC"
    if [ -d "$LIBDIR" ]; then
      export PKG_CONFIG_PATH=$LIBDIR/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}
      export LD_LIBRARY_PATH=$LIBDIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
      export RUSTFLAGS="-L native=$LIBDIR${RUSTFLAGS:+ $RUSTFLAGS}"
    fi
    cargo build --release "${packages[@]}"
  )
fi

mkdir -p "$OPT/lib" "$(dirname "$WRAPPER")"
for b in "${bins[@]}"; do
  # Write to a temp name and rename: a running copy keeps its old inode.
  install -m 755 "$SRC/target/release/$b" "$OPT/$b.new"
  mv -f "$OPT/$b.new" "$OPT/$b"
done

# Only the libraries the binary resolves from the sysroot travel with it.
if [ -d "$LIBDIR" ]; then
  LD_LIBRARY_PATH=$LIBDIR ldd "$OPT/wezterm-gui" "$OPT/wezterm" |
    awk -v root="$SYSROOT/" 'index($3, root) == 1 { print $3 }' | sort -u |
    while read -r lib; do
      cp -fL "$lib" "$OPT/lib/$(basename "$lib")"
    done
fi

cat >"$WRAPPER.new" <<WRAP
#!/usr/bin/env bash
# Hivegrid WezTerm fork. Put its directory first on PATH so \`wezterm cli\` inside
# panes finds the matching binary.
D="$OPT"
export LD_LIBRARY_PATH="\$D/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
export PATH="\$D:\$PATH"
exec "\$D/wezterm-gui" "\$@"
WRAP
chmod 755 "$WRAPPER.new"
mv -f "$WRAPPER.new" "$WRAPPER"

echo "deployed to $OPT; launch with $WRAPPER"
