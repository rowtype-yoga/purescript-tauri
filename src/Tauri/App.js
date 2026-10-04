import { exit, setDockVisibility } from "@tauri-apps/api/app";

export const exitImpl = code => () => exit(code);

export const setDockVisibilityImpl = visible => () => setDockVisibility(visible);
