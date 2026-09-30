-- | Authenticated localhost transport for an application's JSON-RPC handler.
-- | Register tauri-plugin-local-control and grant local-control:allow-start,
-- | local-control:allow-stop, local-control:allow-reply and the core event
-- | listen/unlisten permissions to the owning webview only.
module Tauri.Control (serve) where

import Prelude

import Control.Alt ((<|>))
import Data.Array as Array
import Data.DateTime.Instant (unInstant)
import Data.Either (Either(..))
import Data.Foldable (traverse_)
import Effect (Effect)
import Effect.Aff (Aff, Fiber, Milliseconds(..), apathize, attempt, delay, error, finally, generalBracket, joinFiber, killFiber, launchAff_, launchSuspendedAff, parallel, sequential)
import Effect.Class (liftEffect)
import Effect.Now (now)
import Effect.Ref as Ref
import Promise (Promise)
import Promise.Aff (toAffE)
import Tauri.Internal.Resource (acquireDisposer, once)

type Request =
  { session :: String
  , id :: String
  , kind :: String
  , body :: String
  , deadline :: Number
  }

type Running = { id :: String, fiber :: Fiber Unit }

data SessionState = Starting (Array Request) | Active String | Stopped

derive instance Eq SessionState

type Resources =
  { session :: Ref.Ref SessionState
  , running :: Ref.Ref (Array Running)
  , dispose :: Effect Unit
  }

clock :: Effect Number
clock = do
  Milliseconds value <- unInstant <$> now
  pure value

send :: Request -> String -> String -> Aff Boolean
send request action body = toAffE $ replyImpl
  { session: request.session, id: request.id, action, body }

-- | Subscribe before publishing discovery. The returned, idempotent cleanup
-- | immediately disables dispatch/unsubscribes, stops the native server, and
-- | cancels pending handler fibers. Acquisition cancellation/failure cleans up
-- | even when an uncancellable native registration finishes later.
-- |
-- | Requests have a native deadline and a native claim handshake, so an expired
-- | queued event cannot start a handler. Active handlers are cancelled at their
-- | deadline or when their HTTP request/session ends. Handlers must honour Aff
-- | cancellation; already-completed synchronous effects cannot be rolled back.
serve :: (String -> Aff String) -> Aff (Effect Unit)
serve handler = generalBracket acquire
  { killed: \_ resources -> liftEffect resources.dispose
  , failed: \_ resources -> liftEffect resources.dispose
  , completed: \_ _ -> pure unit
  }
  \resources -> generalBracket (toAffE startImpl)
    { killed: \_ session -> apathize $ toAffE $ stopImpl session
    , failed: \_ session -> apathize $ toAffE $ stopImpl session
    , completed: \_ _ -> pure unit
    }
    \session -> do
      liftEffect do
        previous <- Ref.read resources.session
        Ref.write (Active session) resources.session
        case previous of
          Starting pending -> traverse_ (receive resources.session resources.running) pending
          _ -> pure unit
      pure resources.dispose
  where
  acquire :: Aff Resources
  acquire = do
    session <- liftEffect $ Ref.new (Starting [])
    running <- liftEffect $ Ref.new []
    unlisten <- acquireDisposer $ listenImpl $ receive session running
    dispose <- liftEffect $ once do
      previous <- Ref.read session
      Ref.write Stopped session
      unlisten
      fibers <- Ref.read running
      Ref.write [] running
      case previous of
        Active id -> launchAff_ $ apathize $ toAffE $ stopImpl id
        _ -> pure unit
      traverse_ (\item -> launchAff_ $ apathize $ killFiber (error "Control stopped") item.fiber) fibers
    pure { session, running, dispose }

  receive session running request = do
    current <- Ref.read session
    case current of
      -- Native discovery can be read before the start reply reaches this
      -- webview. Retain those events until their session has been adopted.
      Starting pending -> Ref.write (Starting (Array.snoc pending request)) session
      _ -> when (current == Active request.session) do
        fibers <- Ref.read running
        if request.kind == "cancel" then
          traverse_
            (\item -> launchAff_ $ apathize $ killFiber (error "Control request cancelled") item.fiber)
            (Array.filter (\item -> item.id == request.id) fibers)
        else when (request.kind == "request" && not (Array.any (\item -> item.id == request.id) fibers)) do
          time <- clock
          when (time < request.deadline) do
            fiber <- launchSuspendedAff $ finally
              (liftEffect $ Ref.modify_ (Array.filter (\item -> item.id /= request.id)) running)
              ( apathize $ sequential $
                  parallel (dispatch session request)
                    <|> parallel (delay $ Milliseconds $ request.deadline - time)
              )
            Ref.modify_ (\items -> Array.snoc items { id: request.id, fiber }) running
            launchAff_ $ apathize $ joinFiber fiber

  dispatch session request = do
    claimed <- send request "claim" ""
    current <- liftEffect $ Ref.read session
    time <- liftEffect clock
    when (claimed && current == Active request.session && time < request.deadline) do
      response <- attempt $ handler request.body
      case response of
        Left _ -> void $ send request "fail" ""
        Right body -> void $ send request "reply" body

foreign import listenImpl :: (Request -> Effect Unit) -> Effect (Promise (Effect Unit))
foreign import startImpl :: Effect (Promise String)
foreign import stopImpl :: String -> Effect (Promise Unit)
foreign import replyImpl
  :: { session :: String, id :: String, action :: String, body :: String }
  -> Effect (Promise Boolean)
