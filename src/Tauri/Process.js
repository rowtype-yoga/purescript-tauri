import { invoke } from "@tauri-apps/api/core";

const streamHeader = "x-tauri-bundled-process-stream";

export const runImpl = options => () => invoke("plugin:bundled-process|run", options);

export const spawnImpl = options => () =>
  invoke("plugin:bundled-process|spawn", options);

export const writeImpl = stream => buffer => () =>
  invoke("plugin:bundled-process|write", buffer, {
    headers: { [streamHeader]: stream }
  });

export const finishImpl = stream => () =>
  invoke("plugin:bundled-process|finish", { stream });

export const cancelImpl = stream => () =>
  invoke("plugin:bundled-process|cancel", { stream });

export const stdoutImpl = text => () =>
  invoke("plugin:bundled-process|stdout", { text });

export const stderrImpl = text => () =>
  invoke("plugin:bundled-process|stderr", { text });
