import { invoke } from "@tauri-apps/api/core";

export const argumentsImpl = () => invoke("plugin:launch-file|arguments");

export const authorizeFileArgumentImpl = path => () =>
  invoke("plugin:launch-file|authorize_file_argument", { path });
