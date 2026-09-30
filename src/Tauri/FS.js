import { readTextFile, writeTextFile, watch } from "@tauri-apps/plugin-fs";

export const readTextImpl = path => () => readTextFile(path);

export const writeTextImpl = path => contents => () => writeTextFile(path, contents);

export const watchImpl = path => changed => () =>
  watch(path, () => changed(), { recursive: false });
