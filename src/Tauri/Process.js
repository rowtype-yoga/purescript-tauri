import { invoke } from "@tauri-apps/api/core";

export const runImpl = options => () => invoke("plugin:bundled-process|run", options);
