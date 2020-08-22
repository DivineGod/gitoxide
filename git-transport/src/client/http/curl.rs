use crate::client::http;
use curl::easy::Easy2;
use git_features::pipe;
use std::io::Write;
use std::{
    io,
    sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError},
    thread,
};

#[derive(Default)]
struct Handler {
    send_header: Option<pipe::Writer>,
    send_data: Option<pipe::Writer>,
    receive_body: Option<pipe::Reader>,
}

impl curl::easy::Handler for Handler {
    fn read(&mut self, data: &mut [u8]) -> Result<usize, curl::easy::ReadError> {
        drop(self.send_header.take()); // signal header readers to stop trying
        match self.send_data.as_mut() {
            Some(writer) => writer.write_all(data).map_err(|_| curl::easy::ReadError::Abort)?,
            None => return Err(curl::easy::ReadError::Abort),
        }
        Ok(data.len())
    }

    fn header(&mut self, data: &[u8]) -> bool {
        match self.send_header.as_mut() {
            Some(writer) => writer.write_all(data).is_ok(),
            None => false,
        }
    }
}

pub struct Curl {
    req: SyncSender<Request>,
    res: Receiver<Response>,
    handle: Option<thread::JoinHandle<Result<(), curl::Error>>>,
}

impl Curl {
    pub fn new() -> Self {
        let (handle, req, res) = new_remote_curl();
        Curl {
            handle: Some(handle),
            req,
            res,
        }
    }

    fn make_request(
        &mut self,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<http::PostResponse<pipe::Reader, pipe::Reader, pipe::Writer>, http::Error> {
        let mut list = curl::easy::List::new();
        for header in headers {
            list.append(header.as_ref())?;
        }
        if self
            .req
            .send(Request {
                url: url.to_owned(),
                headers: list,
            })
            .is_err()
        {
            return Err(self.restore_thread_after_failure());
        }
        let Response {
            headers,
            body,
            upload_body,
        } = match self.res.recv() {
            Ok(res) => res,
            Err(_) => return Err(self.restore_thread_after_failure()),
        };
        Ok(http::PostResponse {
            post_body: upload_body,
            headers,
            body,
        })
    }

    fn restore_thread_after_failure(&mut self) -> http::Error {
        let err_that_brought_thread_down = self
            .handle
            .take()
            .expect("thread handle present")
            .join()
            .expect("handler thread should never panic")
            .expect_err("something should have gone wrong with curl (we join on error only)");
        let (handle, req, res) = new_remote_curl();
        self.handle = Some(handle);
        self.req = req;
        self.res = res;
        return err_that_brought_thread_down.into();
    }
}

struct Request {
    url: String,
    headers: curl::easy::List,
}

struct Response {
    headers: pipe::Reader,
    body: pipe::Reader,
    upload_body: pipe::Writer,
}

fn new_remote_curl() -> (
    thread::JoinHandle<Result<(), curl::Error>>,
    SyncSender<Request>,
    Receiver<Response>,
) {
    let (req_send, req_recv) = sync_channel(0);
    let (res_send, res_recv) = sync_channel(0);
    let handle = std::thread::spawn(move || -> Result<(), curl::Error> {
        let mut handle = Easy2::new(Handler::default());
        for Request { url, headers } in req_recv {
            handle.url(&url)?;
            handle.http_headers(headers)?;

            let (receive_data, receive_headers, send_body) = {
                let handler = handle.get_mut();
                let (send, receive_data) = pipe::unidirectional(1);
                handler.send_data = Some(send);
                let (send, receive_headers) = pipe::unidirectional(1);
                handler.send_header = Some(send);
                let (send_body, receive_body) = pipe::unidirectional(None);
                handler.receive_body = Some(receive_body);
                (receive_data, receive_headers, send_body)
            };

            if let Err(err) = handle.perform() {
                let handler = handle.get_mut();
                let err = Err(io::Error::new(io::ErrorKind::Other, err));
                handler.receive_body.take();
                match (handler.send_header.take(), handler.send_data.take()) {
                    (Some(header), mut data) => {
                        if let Err(TrySendError::Disconnected(err)) | Err(TrySendError::Full(err)) =
                            header.channel.try_send(err)
                        {
                            if let Some(body) = data.take() {
                                body.channel.try_send(err).ok();
                            }
                        }
                    }
                    (None, Some(body)) => {
                        body.channel.try_send(err).ok();
                    }
                    (None, None) => {}
                };
            } else {
                let handler = handle.get_mut();
                handler.receive_body.take();
                handler.send_header.take();
                handler.send_data.take();
            }

            if res_send
                .send(Response {
                    headers: receive_headers,
                    body: receive_data,
                    upload_body: send_body,
                })
                .is_err()
            {
                break;
            }
        }
        Ok(())
    });
    (handle, req_send, res_recv)
}

impl From<curl::Error> for http::Error {
    fn from(err: curl::Error) -> Self {
        http::Error::Detail(err.to_string())
    }
}

impl crate::client::http::Http for Curl {
    type Headers = pipe::Reader;
    type ResponseBody = pipe::Reader;
    type PostBody = pipe::Writer;

    fn get(
        &mut self,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<http::GetResponse<Self::Headers, Self::ResponseBody>, http::Error> {
        self.make_request(url, headers).map(Into::into)
    }

    fn post(
        &mut self,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<http::PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>, http::Error> {
        self.make_request(url, headers)
    }
}