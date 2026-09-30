-- | Process startup arguments and file access explicitly authorized by them.
-- | Register tauri-plugin-launch-file alongside tauri-plugin-fs and grant
-- | launch-file:allow-arguments and launch-file:allow-authorize-file-argument
-- | only to the application webview that handles launch arguments.
module Tauri.Launch (arguments, authorizeFileArgument) where

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

foreign import argumentsImpl :: Effect (Promise (Array String))
foreign import authorizeFileArgumentImpl :: String -> Effect (Promise String)
