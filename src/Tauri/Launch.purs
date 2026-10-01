-- | Process startup arguments, standard input, and explicitly authorized files.
-- | Register tauri-plugin-launch-file alongside tauri-plugin-fs and grant
-- | launch-file:allow-arguments, launch-file:allow-authorize-file-argument,
-- | and launch-file:allow-read-stdin only to the application webview that
-- | handles launch input.
module Tauri.Launch (arguments, authorizeFileArgument, readStdin) where

import Effect (Effect)
import Effect.Aff (Aff)
import Promise (Promise)
import Promise.Aff (toAffE)

-- | Arguments captured at plugin setup, excluding the executable name.
arguments :: Aff (Array String)
arguments = toAffE argumentsImpl

-- | Authorize one existing regular file named by an exact launch argument.
-- | Relative paths use the captured startup working directory. Returns the
-- | resolved absolute path; neither its parent nor sibling files are granted.
-- | Filesystem operation permissions are still required separately.
authorizeFileArgument :: String -> Aff String
authorizeFileArgument path = toAffE (authorizeFileArgumentImpl path)

-- | Read UTF-8 standard input to EOF on a native blocking worker.
-- | I/O and invalid UTF-8 errors reject the Aff.
readStdin :: Aff String
readStdin = toAffE readStdinImpl

foreign import argumentsImpl :: Effect (Promise (Array String))
foreign import authorizeFileArgumentImpl :: String -> Effect (Promise String)
foreign import readStdinImpl :: Effect (Promise String)
