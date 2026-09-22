# Cosmic Radio Atlas workspace recipes.
appid := 'io.github.goshitsarcheng.RadioAtlas'
applet-id := 'io.github.goshitsarcheng.RadioAtlasApplet'

# Path to root file system, which defaults to `/`.
rootdir := ''
# The prefix for the `/usr` directory.
prefix := '/usr'
# The location of the cargo target directory.
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')

base-dir := absolute_path(clean(rootdir / prefix))
bin-dst := base-dir / 'bin'
desktop-dst := base-dir / 'share' / 'applications'
appdata-dst := base-dir / 'share' / 'appdata'
icon-svg-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable' / 'apps'
libexec-dst := base-dir / 'lib' / 'cosmic-radio-atlas'

# Default recipe which runs `just build-release`
default: build-release

# Runs `cargo clean`
clean:
    cargo clean

# Compiles with debug profile
build-debug *args:
    cargo build --locked {{args}}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Runs the app for testing purposes
run *args:
    env RUST_BACKTRACE=full cargo run --release --locked -p cosmic-radio-atlas {{args}}

# Runs clippy on the workspace
check *args:
    cargo clippy --workspace --all-targets --locked {{args}} -- -D warnings

# Formats the workspace
fmt:
    cargo fmt --all

# Runs the backend test suites (no display needed)
test:
    cargo test --locked -p radio-core -p radio-player -p radio-tasks

# Installs files
install:
    install -Dm0755 {{ cargo-target-dir / 'release' / 'cosmic-radio-atlas' }} {{ bin-dst / 'cosmic-radio-atlas' }}
    install -Dm0755 {{ cargo-target-dir / 'release' / 'cosmic-applet-radio-atlas' }} {{ bin-dst / 'cosmic-applet-radio-atlas' }}
    install -Dm0644 {{ 'app' / 'target' / 'xdgen' / 'app.desktop' }} {{ desktop-dst / appid + '.desktop' }}
    install -Dm0644 {{ 'app' / 'target' / 'xdgen' / 'app.metainfo.xml' }} {{ appdata-dst / appid + '.metainfo.xml' }}
    install -Dm0644 {{ 'applet' / 'target' / 'xdgen' / 'app.desktop' }} {{ desktop-dst / applet-id + '.desktop' }}
    install -Dm0644 {{ 'applet' / 'target' / 'xdgen' / 'app.metainfo.xml' }} {{ appdata-dst / applet-id + '.metainfo.xml' }}
    install -Dm0644 {{ 'app' / 'resources' / 'icons' / 'hicolor' / 'scalable' / 'apps' / 'icon.svg' }} {{ icon-svg-dst / appid + '.svg' }}
    install -Dm0755 {{ 'crates' / 'radio-player' / 'assets' / 'radio-session' }} {{ libexec-dst / 'radio-session' }}
    install -Dm0755 {{ 'crates' / 'radio-player' / 'assets' / 'radio-sandbox' }} {{ libexec-dst / 'radio-sandbox' }}
    install -Dm0755 {{ 'crates' / 'radio-player' / 'assets' / 'radio-proxy' }} {{ libexec-dst / 'radio-proxy' }}
    install -Dm0644 {{ 'crates' / 'radio-player' / 'assets' / 'radio-status.lua' }} {{ libexec-dst / 'radio-status.lua' }}

# Uninstalls installed files
uninstall:
    rm -f {{ bin-dst / 'cosmic-radio-atlas' }} {{ bin-dst / 'cosmic-applet-radio-atlas' }}
    rm -f {{ desktop-dst / appid + '.desktop' }} {{ desktop-dst / applet-id + '.desktop' }}
    rm -f {{ appdata-dst / appid + '.metainfo.xml' }} {{ appdata-dst / applet-id + '.metainfo.xml' }}
    rm -f {{ icon-svg-dst / appid + '.svg' }}
    rm -rf {{ libexec-dst }}
