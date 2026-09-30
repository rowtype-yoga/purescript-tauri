-- | Requires fs:allow-read-text-file, fs:allow-write-text-file and/or
-- | fs:allow-watch, plus narrowly scoped allowed paths. Watch disposal also
-- | requires core:resources:allow-close. Dialog-picked paths
-- | can be added to the native scope by the dialog plugin; this does not grant
-- | parent-directory access. Register tauri-plugin-fs with its `watch` Cargo
-- | feature for watches. No broad filesystem capability is needed or granted.
module Tauri.FS (readText, writeText, watch) where

import Prelude

import Effect (Effect)
import Effect.Aff (Aff)
import Promise (Promise)
import Promise.Aff (toAffE)
import Tauri.Internal.Resource (acquireDisposer)

readText :: String -> Aff String
readText = toAffE <<< readTextImpl

writeText :: String -> String -> Aff Unit
writeText path contents = toAffE $ writeTextImpl path contents

-- | Watch a file non-recursively using the official debounced watcher (2 s).
-- | The returned, idempotent Effect calls the real native unwatch function.
-- | Cancelling registration also releases the eventual native watch handle.
-- |
-- | A file watch is not a durable path subscription: atomic replacement,
-- | rename or deletion may detach the underlying OS watcher. Applications
-- | requiring continued observation must explicitly re-register after such
-- | changes or arrange a separately scoped parent-directory watch. This API
-- | never widens a selected file's scope to its parent automatically.
-- | Notifications may include this application's own writes.
-- |
-- | Upstream UnwatchFn starts async resource closure but returns void. The
-- | Effect therefore cannot await native cleanup; upstream rejections remain
-- | visible as unhandled promise rejections rather than being swallowed.
watch :: String -> Effect Unit -> Aff (Effect Unit)
watch path changed = acquireDisposer $ watchImpl path changed

foreign import readTextImpl :: String -> Effect (Promise String)
foreign import writeTextImpl :: String -> String -> Effect (Promise Unit)
foreign import watchImpl :: String -> Effect Unit -> Effect (Promise (Effect Unit))
