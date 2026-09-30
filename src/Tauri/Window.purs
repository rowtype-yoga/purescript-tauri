-- | Native operations require core:window:allow-set-title,
-- | core:window:allow-destroy and core:event:allow-listen/allow-unlisten.
module Tauri.Window (CloseRequest, setTitle, onCloseRequested, preventClose, destroy) where

import Prelude

import Effect (Effect)
import Effect.Aff (Aff)
import Promise (Promise)
import Promise.Aff (toAffE)
import Tauri.Internal.Resource (acquireDisposer)

setTitle :: String -> Aff Unit
setTitle = toAffE <<< setTitleImpl

-- | Call preventClose synchronously inside the callback BEFORE launching an
-- | Aff for confirmation/save work. After that work succeeds, call destroy.
-- | Without prevention, Tauri destroys the window when this callback returns.
-- | The returned Effect invokes the real unlisten function at most once.
-- | Cancelling a pending registration releases its eventual listener too.
onCloseRequested :: (CloseRequest -> Effect Unit) -> Aff (Effect Unit)
onCloseRequested handler = acquireDisposer $ onCloseRequestedImpl handler

-- | Destroy immediately, bypassing close-request listeners. Only call this
-- | after the application has resolved any unsaved-work confirmation.
destroy :: Aff Unit
destroy = toAffE destroyImpl

foreign import data CloseRequest :: Type
foreign import preventClose :: CloseRequest -> Effect Unit
foreign import setTitleImpl :: String -> Effect (Promise Unit)
foreign import onCloseRequestedImpl
  :: (CloseRequest -> Effect Unit)
  -> Effect (Promise (Effect Unit))
foreign import destroyImpl :: Effect (Promise Unit)
