use hyper::client::connect::HttpConnector;
use hyper::service::Service;
use hyper::{Body, Client, Request, Response};
use std::future::Future;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use rand::prelude::*;
use rand::rngs::SmallRng;

use hyper_tls::HttpsConnector;

use crate::proxy::middleware::MiddlewareResult::*;
use crate::Middlewares;

// type BoxFut = Box<dyn Future<Output = Result<hyper::Response<Body>, hyper::Error>> + Send>;
pub type State = Arc<Mutex<HashMap<(String, u64), String>>>;

pub struct ProxyService {
    client: Client<HttpsConnector<HttpConnector>>,
    middlewares: Middlewares,
    state: State,
    remote_addr: SocketAddr,
    rng: SmallRng,
}

#[derive(Clone, Copy)]
pub struct ServiceContext {
    pub remote_addr: SocketAddr,
    pub req_id: u64,
}

impl Service<Request<hyper::Body>> for ProxyService {
    type Response = Response<hyper::Body>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.client.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: Request<hyper::Body>) -> Self::Future {
        self.clear_state();
        let (parts, body) = req.into_parts();
        let mut req = Request::from_parts(parts, body);

        // Create references for future callbacks
        // references are moved in each chained future (map,then..)
        let mws_early = Arc::clone(&self.middlewares);
        let mws_failure = Arc::clone(&self.middlewares);
        let mws_success = Arc::clone(&self.middlewares);
        let mws_after_success = Arc::clone(&self.middlewares);
        let mws_after_failure = Arc::clone(&self.middlewares);
        let state_early = Arc::clone(&self.state);
        let state_failure = Arc::clone(&self.state);
        let state_success = Arc::clone(&self.state);
        let state_after_success = Arc::clone(&self.state);
        let state_after_failure = Arc::clone(&self.state);

        let req_id = self.rng.next_u64();

        let context = ServiceContext {
            req_id,
            remote_addr: self.remote_addr,
        };

        let client = self.client.clone();

        Box::pin(async move {
            let mut before_res: Option<Response<Body>> = None;
            for mw in mws_early.lock().await.iter_mut() {
                // Run all middlewares->before_request
                if let Some(res) = match mw.before_request(&mut req, &context, &state_early) {
                    Err(err) => Some(Response::from(err)),
                    Ok(RespondWith(response)) => Some(response),
                    Ok(Next) => None,
                } {
                    // Stop when an early response is wanted
                    before_res = Some(res);
                    break;
                }
            }
            if let Some(mut res) = before_res {
                for mw in mws_early.lock().await.iter_mut() {
                    match mw
                        .after_request(Some(&mut res), &context, &state_early)
                        .await
                    {
                        Err(err) => res = Response::from(err),
                        Ok(RespondWith(response)) => res = response,
                        Ok(Next) => (),
                    }
                }
                debug!("Early response is {:?}", &res);
                return Ok(res);
            }
            let res = match client.request(req).await {
                Err(err) => {
                    for mw in mws_failure.lock().await.iter_mut() {
                        // TODO: think about graceful handling
                        if let Err(err) = mw.request_failure(&err, &context, &state_failure) {
                            error!("Request_failure errored: {:?}", &err);
                        }
                    }
                    Err(err)
                }
                Ok(mut res) => {
                    for mw in mws_success.lock().await.iter_mut() {
                        match mw.request_success(&mut res, &context, &state_success) {
                            Err(err) => res = Response::from(err),
                            Ok(RespondWith(response)) => res = response,
                            Ok(Next) => (),
                        }
                    }
                    Ok(res)
                }
            };
            match res {
                Err(err) => {
                    let mut res = Err(err);
                    for mw in mws_after_success.lock().await.iter_mut() {
                        match mw.after_request(None, &context, &state_after_success).await {
                            Err(err) => res = Ok(Response::from(err)),
                            Ok(RespondWith(response)) => res = Ok(response),
                            Ok(Next) => (),
                        }
                    }
                    res
                }
                Ok(mut res) => {
                    for mw in mws_after_failure.lock().await.iter_mut() {
                        match mw
                            .after_request(Some(&mut res), &context, &state_after_failure)
                            .await
                        {
                            Err(err) => res = Response::from(err),
                            Ok(RespondWith(response)) => res = response,
                            Ok(Next) => (),
                        }
                    }
                    Ok(res)
                }
            }
        })
    }
}

impl ProxyService {
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

    pub fn new(middlewares: Middlewares, remote_addr: SocketAddr) -> Self {
        let https = HttpsConnector::new();
        let client = Client::builder().build::<_, hyper::Body>(https);
        ProxyService {
            client,
            state: Arc::new(Mutex::new(HashMap::new())),
            rng: SmallRng::from_entropy(),
            remote_addr,
            middlewares,
        }
    }
}
