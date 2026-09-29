//! Embeds the app icon into `zuno.exe` on Windows. Nothing happens anywhere else.
//!
//! **gpui reads the window and taskbar icon out of the executable** — resource #1 of type ICON,
//! in `load_icon` — so on Windows the icon is a build artifact, not a packaging one as it is for
//! the `.desktop` on Linux or the `.icns` on macOS. Without this, every Zuno window is the generic
//! program icon.
//!
//! **Drawn from `assets/icons/zuno.svg` at build time** rather than committed as an `.ico`, so
//! replacing the placeholder SVG stays the whole job on every platform, with no raster copy to go
//! stale. With gpui's own resvg, the same renderer the icon tests hold the SVG to.
//!
//! gpui's own resource #1 is its manifest, a different resource type, so the two do not collide.

fn main() {
    #[cfg(target_os = "windows")]
    windows::embed_icon();
}

#[cfg(target_os = "windows")]
mod windows {
    use std::path::PathBuf;

    /// Every size Windows asks an `.ico` for: 16–32 for title bars and lists, 48 for Explorer,
    /// 256 for large icons and the Start menu.
    const SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];

    pub fn embed_icon() {
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
        let svg = manifest_dir.join("../assets/icons/zuno.svg");
        println!("cargo:rerun-if-changed={}", svg.display());

        let data = std::fs::read(&svg).expect("read assets/icons/zuno.svg");
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default())
            .expect("zuno.svg parses");

        let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
        for size in SIZES {
            let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("pixmap");
            let scale = size as f32 / tree.size().width().max(tree.size().height());
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            // tiny-skia stores premultiplied alpha; an `.ico` holds straight RGBA, and a
            // premultiplied edge would draw as a dark fringe.
            let rgba = pixmap
                .pixels()
                .iter()
                .flat_map(|pixel| {
                    let color = pixel.demultiply();
                    [color.red(), color.green(), color.blue(), color.alpha()]
                })
                .collect();
            let image = ico::IconImage::from_rgba_data(size, size, rgba);
            icon.add_entry(ico::IconDirEntry::encode(&image).expect("encode icon"));
        }

        let out = PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));
        let ico_path = out.join("zuno.ico");
        let file = std::fs::File::create(&ico_path).expect("create zuno.ico");
        icon.write(file).expect("write zuno.ico");

        // `/` in the path: the resource compiler reads `\` inside a string as an escape.
        let rc_path = out.join("zuno.rc");
        let ico = ico_path.display().to_string().replace('\\', "/");
        std::fs::write(&rc_path, format!("1 ICON \"{ico}\"\n")).expect("write zuno.rc");
        // `required`, whatever the name says: `manifest_optional` treats "no resource compiler
        // found" as success, which would ship an exe with the generic icon and no error.
        embed_resource::compile(&rc_path, embed_resource::NONE)
            .manifest_required()
            .expect("embed the icon");
    }
}
