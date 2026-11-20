use futures::{
    FutureExt,
    future::BoxFuture,
};

use super::*;

pub struct Route<C, K> {
    client: C,
    kind: K,
}

impl<C, K> Route<C, K>
where
    C: ClientPrelude,
{
    pub fn new(client: &C, kind: K) -> Self {
        Self {
            client: client.clone(),
            kind,
        }
    }
}

impl<C, Re: Request> std::fmt::Display for Route<C, Re> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str(&self.kind.path())
    }
}

impl<C, Re> IntoFuture for Route<C, Re>
where
    C: ClientPrelude,
    Re: Request,
{
    type Output = Result<Re::Response>;

    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        async move {
            let path = self.kind.path();
            let path = path.strip_prefix('/').unwrap_or(&path);
            let base_uri = self.client.base_uri().trim_end_matches('/');
            let client = self.client.client();
            let mut request = client.request(Re::METHOD, format!("{base_uri}/{path}"));

            if let Some(body) = self.kind.body() {
                request = request.json(&body);
            }

            if let Some(params) = self.kind.params() {
                request = request.query(&params);
            }

            let mut request = request.build().map_err(Error::HttpError)?;
            if let Some(headers) = self.client.headers(request.url().path()) {
                request.headers_mut().extend(headers);
            }

            let response = client.execute(request).await.map_err(Error::HttpError)?;

            let data = response
                .json::<Re::Response>()
                .await
                .map_err(Error::HttpError)?;

            Ok(data)
        }
        .boxed()
    }
}
