use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader};
use serde::Deserialize;
use std::fmt::Display;
use std::io::Cursor;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use worker::*;

// Cache sprite sheets locally to help avoid ooms when trying to serve many sprites from the same sheet at once
static CACHE: OnceLock<quick_cache::sync::Cache<String, Arc<DynamicImage>>> = OnceLock::new();

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    process_image(req, env).await.unwrap_or_else(|r| r)
}

async fn process_image(
    req: Request,
    env: Env,
) -> std::result::Result<Result<Response>, Result<Response>> {
    if req.path().ends_with("/") {
        let mut url = Url::from_str(&env.var("BROWSER").map_err(err)?.to_string()).map_err(err)?;
        if let Ok(mut path) = url.path_segments_mut() {
            path.extend(
                req.url()
                    .map_err(err)?
                    .path_segments()
                    .unwrap_or("".split('/')),
            );
        };
        return Ok(Response::redirect(url));
    }

    let params: Params = req.query().map_err(err)?;

    let out_format = params
        .format
        .and_then(ImageFormat::from_extension)
        .or_else(|| {
            if let Ok(Some(accept)) = req.headers().get("accept") {
                for mut v in accept.split(",") {
                    // should parse the `;q=` params instead of ignoring them
                    // https://www.rfc-editor.org/rfc/rfc9110#name-accept-language
                    // "some recipients ... cannot be relied upon" <- that's us
                    v = v.split_once(';').map_or(v, |(s, _)| s).trim();
                    if let Some(f) = ImageFormat::from_mime_type(v) {
                        return Some(f);
                    }
                }
            }
            None
        })
        .unwrap_or(ImageFormat::Png);

    let mut headers = Headers::new();

    let mut filename = Path::new(req.url().map_err(err)?.path()).to_path_buf();
    if let Some(ext) = out_format.extensions_str().iter().next() {
        filename.set_extension(ext);
    }
    if let Some(mut f) = filename
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
    {
        f.retain(|c| c.is_ascii() && !c.is_ascii_control());
        let _ = headers.set(
            "content-disposition",
            format!(r#"inline; filename="{}""#, f).as_str(),
        );
    }
    headers
        .set("content-type", out_format.to_mime_type())
        .map_err(err)?;

    for (name, value) in [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "*"),
        ("Access-Control-Max-Age", "86400"),
        ("Access-Control-Allow-Headers", "*"),
    ] {
        headers.set(name, value).map_err(err)?
    }

    // both the incoming accept-encoding header and the actual encoding of the outgoing file are modified by cloudflare.
    // just need to add the incoming header to our output headers to enable cf to compress the data
    // https://community.cloudflare.com/t/worker-doesnt-return-gzip-brotli-compressed-data/337644/3
    if let Some(encoding) = req
        .headers()
        .get("accept-encoding")
        .map_err(err)?
        .as_ref()
        .and_then(|v| v.split(',').map(str::trim).next())
    {
        headers.set("content-encoding", encoding).ok();
    }

    let mut output = Cursor::new(Vec::new());
    if let Params {
        x: Some(x),
        y: Some(y),
        w: Some(w),
        h: Some(h),
        ..
    } = params
    {
        let cache = CACHE.get_or_init(|| quick_cache::sync::Cache::new(4));
        let image = match cache.get_value_or_guard_async(&req.path()).await {
            Ok(image) => image,
            Err(guard) => {
                let image = Arc::new(get_image(req, env, &mut headers).await?);
                let _ = guard.insert(image.clone());
                image
            }
        };
        let cropped = image.view(x, y, w, h);
        if let Err(e) = cropped.to_image().write_to(&mut output, out_format) {
            return Err(Response::error(
                format!("Failed to write cropped image: {}", e),
                500,
            ));
        }
    } else if let Err(e) = get_image(req, env, &mut headers)
        .await?
        .write_to(&mut output, out_format)
    {
        return Err(Response::error(
            format!("Failed to write image: {}", e),
            500,
        ));
    }

    Ok(Ok(ResponseBuilder::new()
        .with_headers(headers)
        .fixed(output.into_inner())))
}

async fn get_image(
    req: Request,
    env: Env,
    headers: &mut Headers,
) -> std::result::Result<DynamicImage, Result<Response>> {
    let mut response = env
        .service("upstream")
        .map_err(err)?
        .fetch_request(req)
        .await
        .map_err(err)?;
    if response.status_code() >= 400 {
        return Err(Response::error(
            format!("{} error from upstream", response.status_code()),
            500,
        ));
    } else if response.status_code() == 304 {
        return Err(Ok(response));
    }

    for header in ["last-modified", "etag", "cache-control", "expires", "date"] {
        response
            .headers()
            .get(header)
            .map_err(err)?
            .and_then(|v| headers.set(header, v.as_str()).ok());
    }

    let raw = response.bytes().await.map_err(err)?;
    let image = match ImageReader::new(Cursor::new(raw)).with_guessed_format() {
        Err(e) => {
            return Err(Response::error(
                format!("Failed to guess format: {}", e),
                500,
            ))
        }
        Ok(reader) => match reader.decode() {
            Err(e) => {
                return Err(Response::error(
                    format!("Failed to decode image: {}", e),
                    500,
                ))
            }
            Ok(image) => image,
        },
    };
    Ok(image)
}

fn err(e: impl Display) -> Result<Response> {
    Response::error(format!("Error {}", e), 500)
}

#[derive(Deserialize)]
struct Params {
    format: Option<String>,
    x: Option<u32>,
    y: Option<u32>,
    w: Option<u32>,
    h: Option<u32>,
}
