use std::{env, fs, path::Path};
use xdgen::{App, Context, FluentString};

fn main() {
    let ctx = Context::new("i18n", env::var("CARGO_PKG_NAME").unwrap()).unwrap();
    let app = App::new(FluentString("app-title"))
        .comment(FluentString("app-comment"))
        .keywords(FluentString("app-keywords"));

    let desktop_entry = app.expand_desktop("resources/app.desktop", &ctx).unwrap();
    let metainfo = app
        .expand_metainfo("resources/app.metainfo.xml", &ctx)
        .unwrap();

    // Package-local output: in a workspace the shared target dir is not
    // addressable by a fixed relative path, so generate next to the manifest.
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/xdgen");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("app.desktop"), desktop_entry).unwrap();
    fs::write(output.join("app.metainfo.xml"), metainfo).unwrap();
    println!("cargo:rerun-if-changed=resources/app.desktop");
    println!("cargo:rerun-if-changed=resources/app.metainfo.xml");
    println!("cargo:rerun-if-changed=i18n");
}
