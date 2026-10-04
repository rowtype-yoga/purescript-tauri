fn main() {
    tauri_plugin::Builder::new(&["run", "spawn", "write", "finish", "cancel"]).build();
}
