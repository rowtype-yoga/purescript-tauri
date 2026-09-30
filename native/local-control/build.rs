fn main() {
    tauri_plugin::Builder::new(&["start", "stop", "reply"]).build();
}
