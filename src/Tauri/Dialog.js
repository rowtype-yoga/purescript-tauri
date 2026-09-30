import { open, save, message } from "@tauri-apps/plugin-dialog";

export const openFileImpl = options => () =>
  open({ ...options, multiple: false, directory: false });

export const saveFileImpl = options => () => save(options);

export const chooseImpl = text => options => () => message(text, options);
