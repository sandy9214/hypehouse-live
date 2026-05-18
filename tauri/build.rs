// Tauri's build script generates the platform-specific glue
// (Info.plist, manifest, .desktop file, etc.) from tauri.conf.json.
//
// Kept minimal: every Tauri v2 app needs exactly this in build.rs unless
// you have custom resource embeds — we don't (yet).

fn main() {
    tauri_build::build();
}
