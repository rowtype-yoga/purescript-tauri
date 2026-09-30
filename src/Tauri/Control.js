import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";

export const listenImpl = handler => () =>
  getCurrentWebview().listen("local-control://request", event => handler(event.payload)());

export const startImpl = () => invoke("plugin:local-control|start");

export const stopImpl = session => () => invoke("plugin:local-control|stop", { session });

export const replyImpl = request => () => invoke("plugin:local-control|reply", request);
