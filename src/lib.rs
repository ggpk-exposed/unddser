use image::{GenericImage, ImageFormat, ImageReader};
use serde::Deserialize;
use std::io::Cursor;
use std::path::Path;
use std::str::FromStr;
use worker::*;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    if req.path().ends_with("/") {
        let mut url = Url::from_str(&env.var("BROWSER")?.to_string())?;
        if let Ok(mut path) = url.path_segments_mut() {
            path.extend(req.url()?.path_segments().unwrap_or("".split('/')));
        };
        return Response::redirect(url);
    }

    let params: Params = req.query()?;

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
                    if let Some(f) = ImageFormat::from_mime_type(&v) {
                        return Some(f);
                    }
                }
            }
            None
        })
        .unwrap_or(ImageFormat::Png);

    let mut headers = Headers::new();

    let mut filename = Path::new(req.url()?.path()).to_path_buf();
    out_format.extensions_str().iter().next().map(|ext| {
        filename.set_extension(ext);
    });
    filename
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .map(|mut f| {
            f.retain(|c| c.is_ascii() && !c.is_ascii_control());
            let _ = headers.set(
                "content-disposition",
                format!(r#"inline; filename="{}""#, f).as_str(),
            );
        });
    headers.set("content-type", out_format.to_mime_type())?;

    // both the incoming accept-encoding header and the actual encoding of the outgoing file are modified by cloudflare.
    // just need to add the incoming header to our output headers to enable cf to compress the data
    // https://community.cloudflare.com/t/worker-doesnt-return-gzip-brotli-compressed-data/337644/3
    if let Some(encoding) = req
        .headers()
        .get("accept-encoding")?
        .as_ref()
        .and_then(|v| v.split(',').map(str::trim).next())
    {
        headers.set("content-encoding", encoding).ok();
    }

    let mut response = env.service("upstream")?.fetch_request(req).await?;
    if response.status_code() >= 400 {
        return Response::error(
            format!("{} error from upstream", response.status_code()),
            500,
        );
    } else if response.status_code() == 304 {
        return Ok(response);
    }

    for header in ["last-modified", "etag", "cache-control", "expires", "date"] {
        response
            .headers()
            .get(header)?
            .and_then(|v| headers.set(header, v.as_str()).ok());
    }

    for (name, value) in [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "*"),
        ("Access-Control-Max-Age", "86400"),
        ("Access-Control-Allow-Headers", "*"),
    ] {
        headers.set(name, value)?
    }

    let raw = response.bytes().await?;
    let mut image = match ImageReader::new(Cursor::new(raw)).with_guessed_format() {
        Err(e) => return Response::error(format!("Failed to guess format: {}", e), 500),
        Ok(reader) => match reader.decode() {
            Err(e) => return Response::error(format!("Failed to decode image: {}", e), 500),
            Ok(image) => image,
        },
    };

    let mut output = Cursor::new(Vec::new());
    if let Params {
        x: Some(x),
        y: Some(y),
        w: Some(w),
        h: Some(h),
        ..
    } = params
    {
        let cropped = image.sub_image(x, y, w, h);
        if let Err(e) = cropped.to_image().write_to(&mut output, out_format) {
            return Response::error(format!("Failed to write cropped image: {}", e), 500);
        }
    } else {
        if let Err(e) = image.write_to(&mut output, out_format) {
            return Response::error(format!("Failed to write image: {}", e), 500);
        }
    }

    Ok(ResponseBuilder::new()
        .with_headers(headers)
        .fixed(output.into_inner()))
}

#[derive(Deserialize)]
struct Params {
    format: Option<String>,
    x: Option<u32>,
    y: Option<u32>,
    w: Option<u32>,
    h: Option<u32>,
}
