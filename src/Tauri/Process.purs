-- | Run explicitly allowlisted executables beside the native application.
-- | Register tauri-plugin-bundled-process and grant its command permissions only
-- | to the webview that owns a process stream. `stdout` and `stderr` require
-- | bundled-process:allow-stdout and bundled-process:allow-stderr respectively.
module Tauri.Process (Result, Stream, run, spawn, write, finish, cancel, stdout, stderr) where

import Data.ArrayBuffer.Types (ArrayBuffer)
import Effect (Effect)
import Effect.Aff (Aff)
import Prelude (Unit)
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

-- | Start an allowlisted child with open stdin. The stream is usable only by the
-- | webview that created it.
spawn :: { executable :: String, arguments :: Array String } -> Aff Stream
spawn options = toAffE (spawnImpl options)

-- | Write one raw ArrayBuffer to child stdin. Awaiting this Aff acknowledges that
-- | all bytes were accepted by the child pipe. At most one write may be in flight.
-- | Requires the octet-stream IPC transport; JSON/postMessage fallback is rejected.
write :: Stream -> ArrayBuffer -> Aff Unit
write stream buffer = toAffE (writeImpl stream buffer)

-- | Close stdin, wait for the child, and collect its output. Nonzero exits return
-- | a Result; native failures reject the Aff.
finish :: Stream -> Aff Result
finish stream = toAffE (finishImpl stream)

-- | Kill the child and reap it. This does not return the child's output.
cancel :: Stream -> Aff Unit
cancel stream = toAffE (cancelImpl stream)

-- | Write UTF-8 text to the native application's standard output and flush it.
-- | Native I/O failures reject the Aff.
stdout :: String -> Aff Unit
stdout text = toAffE (stdoutImpl text)

-- | Write UTF-8 text to the native application's standard error and flush it.
-- | Native I/O failures reject the Aff.
stderr :: String -> Aff Unit
stderr text = toAffE (stderrImpl text)

-- | An opaque stream owned by the webview that spawned it.
foreign import data Stream :: Type

foreign import runImpl :: { executable :: String, arguments :: Array String, stdin :: String } -> Effect (Promise Result)
foreign import spawnImpl :: { executable :: String, arguments :: Array String } -> Effect (Promise Stream)
foreign import writeImpl :: Stream -> ArrayBuffer -> Effect (Promise Unit)
foreign import finishImpl :: Stream -> Effect (Promise Result)
foreign import cancelImpl :: Stream -> Effect (Promise Unit)
foreign import stdoutImpl :: String -> Effect (Promise Unit)
foreign import stderrImpl :: String -> Effect (Promise Unit)
