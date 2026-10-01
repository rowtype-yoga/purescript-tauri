-- | Run an explicitly allowlisted executable beside the native application.
-- | Register tauri-plugin-bundled-process and grant bundled-process:allow-run
-- | only to the webview that needs to launch bundled programs.
module Tauri.Process (Result, run) where

import Effect (Effect)
import Effect.Aff (Aff)
import Promise (Promise)
import Promise.Aff (toAffE)

type Result =
  { success :: Boolean
  , code :: Int
  , stdout :: String
  , stderr :: String
  }

-- | Send UTF-8 input, close stdin, and collect both output streams concurrently.
-- | Nonzero child exits return a Result; host and spawn failures reject the Aff.
-- | A code of -1 means termination without a numeric exit status. Output bytes
-- | that are not valid UTF-8 are replaced with the Unicode replacement character.
run :: { executable :: String, arguments :: Array String, stdin :: String } -> Aff Result
run options = toAffE (runImpl options)

foreign import runImpl :: { executable :: String, arguments :: Array String, stdin :: String } -> Effect (Promise Result)
