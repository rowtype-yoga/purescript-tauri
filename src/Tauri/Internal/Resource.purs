module Tauri.Internal.Resource (acquireDisposer, once) where

import Prelude

import Effect (Effect)
import Effect.Aff (Aff, generalBracket)
import Effect.Class (liftEffect)
import Effect.Ref as Ref
import Promise (Promise)
import Promise.Aff (toAffE)

-- A pending native registration cannot be aborted. If the Aff is cancelled,
-- wait for its real unlisten handle and dispose it rather than leaking it.
acquireDisposer :: Effect (Promise (Effect Unit)) -> Aff (Effect Unit)
acquireDisposer acquire = generalBracket
  (toAffE acquire >>= liftEffect <<< once)
  { killed: \_ dispose -> liftEffect dispose
  , failed: \_ dispose -> liftEffect dispose
  , completed: \_ _ -> pure unit
  }
  pure

once :: Effect Unit -> Effect (Effect Unit)
once action = do
  disposed <- Ref.new false
  pure do
    alreadyDisposed <- Ref.read disposed
    unless alreadyDisposed do
      Ref.write true disposed
      action
