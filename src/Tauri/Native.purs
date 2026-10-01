-- | Synchronous native primitives provided by tauri-native-host. This module
-- | requires the embedded native host; it does not emulate browser or Node APIs.
module Tauri.Native
  ( Metrics
  , ProcessResult
  , Video
  , arguments
  , version
  , isStdinTTY
  , readStdin
  , readText
  , writeText
  , stdout
  , stderr
  , absolutePath
  , environment
  , monotonicMilliseconds
  , runApplication
  , measureText
  , beginVideo
  , writeVideoFrame
  , finishVideo
  , cancelVideo
  , enterTerminal
  , restoreTerminal
  , terminalSize
  , readKey
  , sleepMilliseconds
  ) where

import Prelude

import Data.Maybe (Maybe)
import Data.Nullable (Nullable, toNullable)
import Effect (Effect)
import Tauri.Native.Canvas (Surface)

type Metrics = { width :: Number, ascent :: Number, descent :: Number }

type ProcessResult = { success :: Boolean, code :: Int, stderr :: String }

-- | Launch the configured sibling application with literal arguments. Source
-- | data, when present, is sent through stdin and never becomes an argument.
-- | Output is inherited; interrupting this wait does not terminate the app.
runApplication :: Array String -> Maybe String -> Effect ProcessResult
runApplication args input = runApplicationImpl args (toNullable input)

foreign import data Video :: Type

foreign import arguments :: Effect (Array String)
foreign import version :: Effect String
foreign import isStdinTTY :: Effect Boolean
foreign import readStdin :: Effect String
foreign import readText :: String -> Effect String
foreign import writeText :: String -> String -> Effect Unit
foreign import stdout :: String -> Effect Unit
foreign import stderr :: String -> Effect Unit
foreign import absolutePath :: String -> Effect String
foreign import environment :: String -> Effect String
foreign import monotonicMilliseconds :: Effect Number
foreign import runApplicationImpl :: Array String -> Nullable String -> Effect ProcessResult
foreign import measureText :: String -> String -> Effect Metrics
foreign import beginVideo :: { output :: String, fps :: Int, width :: Int, height :: Int } -> Effect Video
foreign import writeVideoFrame :: Video -> Surface -> Effect Unit
foreign import finishVideo :: Video -> Effect Unit
foreign import cancelVideo :: Video -> Effect Unit
foreign import enterTerminal :: Effect Unit
foreign import restoreTerminal :: Effect Unit
foreign import terminalSize :: Effect { width :: Int, height :: Int }
foreign import readKey :: Effect String
foreign import sleepMilliseconds :: Int -> Effect Unit
