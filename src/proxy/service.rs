use futures::future;
use futures::future::IntoFuture;

use hyper;
use hyper::client::connect::HttpConnector;
use hyper::rt::Future;
use hyper::service::{NewService, Service};
use hyper::{Body, Client, Request, Response};

use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex};

use rand::prelude::*;
use rand::rngs::SmallRng;
use rand::FromEntropy;

use proxy::middleware::MiddlewareResult::*;
use Middlewares;

type BoxFut = Box<Future<Item = hyper::Response<Body>, Error = hyper::Error> + Send>;
pub type State = Arc<Mutex<HashMap<(String, u64), String>>>;

pub struct ProxyService {
  client: Client<HttpConnector, Body>,
  middlewares: Middlewares,
  state: State,
  rng: SmallRng,
}

impl Service for ProxyService {
  type Error = hyper::Error;
  type Future = BoxFut;
  type ReqBody = Body;
  type ResBody = Body;

  fn call(&mut self, req: Request<Self::ReqBody>) -> Self::Future {
    self.clear_state();
    let (parts, body) = req.into_parts();
    let mut req = Request::from_parts(parts, body);

    // Create references for future callbacks
    // references are moved in each chained future (map,then..)
    let mws_failure = Arc::clone(&self.middlewares);
    let mws_success = Arc::clone(&self.middlewares);
    let mws_after = Arc::clone(&self.middlewares);
    let state_failure = Arc::clone(&self.state);
    let state_success = Arc::clone(&self.state);
    let state_after = Arc::clone(&self.state);

    let req_id = self.rng.next_u64();

    let mut before_res: Option<Response<Body>> = None;
    for mw in self.middlewares.lock().unwrap().iter_mut() {
      // Run all middlewares->before_request
      if let Some(res) = match mw.before_request(&mut req, req_id, &self.state) {
        Err(err) => Some(Response::from(err)),
        Ok(RespondWith(response)) => Some(response),
        Ok(Next) => None,
      } {
        // Stop when an early response is wanted
        before_res = Some(res);
        break;
      }
    }

    if let Some(res) = before_res {
      return Box::new(future::ok(self.early_response(req_id, res, &self.state)));
    }

    let res = self
      .client
      .request(req)
      .map_err(move |err| {
        for mw in mws_failure.lock().unwrap().iter_mut() {
          // TODO: think about graceful handling
          if let Err(err) = mw.request_failure(&err, req_id, &state_failure) {
            error!("Request_failure errored: {:?}", &err);
          }
        }
        err
      })
      .map(move |mut res| {
        for mw in mws_success.lock().unwrap().iter_mut() {
          match mw.request_success(&mut res, req_id, &state_success) {
            Err(err) => res = Response::from(err),
            Ok(RespondWith(response)) => res = response,
            Ok(Next) => (),
          }
        }
        res
      })
      .then(move |mut res| {
        for mw in mws_after.lock().unwrap().iter_mut() {
          match mw.after_request(req_id, &state_after) {
            Err(err) => res = Ok(Response::from(err)),
            Ok(RespondWith(response)) => res = Ok(response),
            Ok(Next) => (),
          }
        }
        res
      });

    Box::new(res)
  }
}

impl ProxyService {
  fn early_response(&self, req_id: u64, mut res: Response<Body>, state: &State) -> Response<Body> {
    for mw in self.middlewares.lock().unwrap().iter_mut() {
      match mw.after_request(req_id, state) {
        Err(err) => res = Response::from(err),
        Ok(RespondWith(response)) => res = response,
        Ok(Next) => (),
      }
    }
    debug!("Early response is {:?}", &res);
    res
  }

  // Needed to avoid a single connection creating too much data in state
  // Since we need to identify each request in state (HashMap tuple identifier), it grows
  // for each request from the same connection
  fn clear_state(&self) {
    if let Ok(mut state) = self.state.lock() {
      state.clear();
    } else {
      error!("[FATAL] Cannot lock state in clean_stale_state");
    }
  }

  pub fn new(middlewares: Middlewares) -> Self {
    ProxyService {
      state: Arc::new(Mutex::new(HashMap::new())),
      client: Client::new(),
      rng: SmallRng::from_entropy(),
      middlewares,
    }
  }
}

impl IntoFuture for ProxyService {
  type Future = future::FutureResult<Self::Item, Self::Error>;
  type Item = Self;
  type Error = hyper::Error;

  fn into_future(self) -> Self::Future {
    future::ok(self)
  }
}

pub struct ProxyServiceBuilder {
  middlewares: Middlewares,
}

impl ProxyServiceBuilder {
  pub fn new(middlewares: Middlewares) -> Self {
    ProxyServiceBuilder { middlewares }
  }
}

impl NewService for ProxyServiceBuilder {
  type Error = hyper::Error;
  type ReqBody = Body;
  type ResBody = Body;
  type Service = ProxyService;
  type InitError = Box<Error + Send + Sync>;
  type Future = Box<Future<Item = Self::Service, Error = Self::InitError> + Send>;

  fn new_service(&self) -> Self::Future {
    let mws = Arc::clone(&self.middlewares);
    Box::new(future::ok(ProxyService::new(mws)))
  }
}
