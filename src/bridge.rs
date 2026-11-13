use serde::Deserialize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Debug log to both stderr and `/tmp/apple-music-gpui.log`.
pub fn dbg_log(msg: &str) {
    use std::io::Write;
    eprintln!("{msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/apple-music-gpui.log")
    {
        let _ = writeln!(f, "{msg}");
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "payload")]
pub enum BridgeEvent {
    #[serde(rename = "ready")]
    Ready {},
    #[serde(rename = "authorized")]
    Authorized {
        #[serde(rename = "userToken")]
        user_token: String,
    },
    #[serde(rename = "storefront_detected")]
    StorefrontDetected { storefront: String },
    #[serde(rename = "playback_state")]
    PlaybackState {
        state: u8,
        #[serde(rename = "isPlaying", default)]
        is_playing: bool,
    },
    #[serde(rename = "playback_time")]
    PlaybackTime { current: f64, duration: f64 },
    #[serde(rename = "fallback_preview")]
    FallbackPreview { url: String, reason: String },
    #[serde(rename = "error")]
    Error { message: String },
}

/// Localhost HTTP bridge server hosting `bridge.html` and `/amp-proxy` for `gpui-webview`
/// MusicKit JS v3 OAuth and full-song FairPlay EME playback.
pub struct EphemeralAuthServer {
    port: u16,
    stop_flag: Arc<AtomicBool>,
    state: Arc<Mutex<(String, String, String)>>, // (dev_token, storefront, existing_user_token)
}

impl EphemeralAuthServer {
    pub fn start(
        initial_dev_token: String,
        storefront: String,
        existing_user_token: String,
    ) -> Result<Self, String> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| format!("Failed to bind localhost bridge server: {e}"))?;

        let port = server
            .server_addr()
            .to_ip()
            .map(|a| a.port())
            .ok_or_else(|| "Failed to resolve localhost bridge port".to_string())?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let state = Arc::new(Mutex::new((
            initial_dev_token,
            storefront,
            existing_user_token,
        )));
        let state_clone = state.clone();

        let http_client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15")
            .build()
            .unwrap_or_default();

        std::thread::Builder::new()
            .name("musickit-bridge-http".to_string())
            .spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    if let Ok(Some(req)) = server.recv_timeout(Duration::from_millis(150)) {
                        let (dev_tok, sf, user_tok) = state_clone
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_else(|_| (String::new(), "us".to_string(), String::new()));

                        if req.url().starts_with("/amp-proxy") {
                            let client = http_client.clone();
                            std::thread::spawn(move || {
                                handle_amp_proxy_request(req, client, dev_tok, sf, user_tok);
                            });
                        } else {
                            let html = render_bridge_html_with_context(&dev_tok, &sf, &user_tok);
                            let header = tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"text/html; charset=utf-8"[..],
                            )
                            .unwrap();
                            let response =
                                tiny_http::Response::from_string(html).with_header(header);
                            let _ = req.respond(response);
                        }
                    }
                }
            })
            .map_err(|e| format!("Failed to spawn bridge server thread: {e}"))?;

        Ok(Self {
            port,
            stop_flag,
            state,
        })
    }

    pub fn bridge_url(&self) -> String {
        format!("http://127.0.0.1:{}/bridge.html", self.port)
    }

    pub fn set_context(&self, dev_token: String, storefront: String, user_token: String) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = (dev_token, storefront, user_token);
        }
    }
}

impl Drop for EphemeralAuthServer {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

fn cors_headers() -> Vec<tiny_http::Header> {
    vec![
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"*"[..]).unwrap(),
        tiny_http::Header::from_bytes(
            &b"Access-Control-Allow-Methods"[..],
            &b"GET, POST, PUT, DELETE, OPTIONS"[..],
        )
        .unwrap(),
    ]
}

fn storefront_fallback_json(sf: &str) -> String {
    let code = if sf.trim().is_empty() {
        "us".to_string()
    } else {
        sf.trim().to_lowercase()
    };
    let lang = match code.as_str() {
        "tw" => "zh-Hant-TW",
        "hk" => "zh-Hant-HK",
        "jp" => "ja-JP",
        "kr" => "ko-KR",
        "gb" => "en-GB",
        _ => "en-US",
    };
    format!(
        r#"{{"data":[{{"id":"{}","type":"storefronts","attributes":{{"defaultLanguageTag":"{}","name":"{}","supportedLanguageTags":["{}","en-US"]}}}}]}}"#,
        code,
        lang,
        code.to_uppercase(),
        lang
    )
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(d1), Some(d2)) = (h1, h2) {
                out.push(((d1 << 4) | d2) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle_amp_proxy_request(
    mut req: tiny_http::Request,
    client: reqwest::blocking::Client,
    dev_tok: String,
    sf: String,
    user_tok: String,
) {
    if req.method().as_str().eq_ignore_ascii_case("OPTIONS") {
        let mut resp = tiny_http::Response::from_data(Vec::new()).with_status_code(200);
        for h in cors_headers() {
            resp.add_header(h);
        }
        let _ = req.respond(resp);
        return;
    }

    let raw_url = req.url().to_string();
    let encoded_target = raw_url
        .split("url=")
        .nth(1)
        .unwrap_or_default()
        .split('&')
        .next()
        .unwrap_or_default();
    let target_url = percent_decode(encoded_target);

    if !target_url.starts_with("https://") || !target_url.contains(".apple.com") {
        let resp = tiny_http::Response::from_string(r#"{"error":"Invalid proxy target"}"#)
            .with_status_code(400);
        let _ = req.respond(resp);
        return;
    }

    let is_storefront_req = target_url.contains("/v1/me/storefront");

    let method = match req.method().as_str().to_uppercase().as_str() {
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "HEAD" => reqwest::Method::HEAD,
        _ => reqwest::Method::GET,
    };

    let mut incoming_auth: Option<String> = None;
    let mut incoming_user_tok: Option<String> = None;
    let mut incoming_content_type: Option<String> = None;
    let mut incoming_accept: Option<String> = None;
    let mut extra_headers: Vec<(String, String)> = Vec::new();

    for h in req.headers() {
        let name = h.field.as_str().as_str().to_lowercase();
        let val = h.value.as_str().to_string();
        match name.as_str() {
            "authorization" => incoming_auth = Some(val),
            "media-user-token" | "music-user-token" | "x-apple-music-user-token" => {
                if !val.trim().is_empty() {
                    incoming_user_tok = Some(val);
                }
            }
            "content-type" => incoming_content_type = Some(val),
            "accept" => incoming_accept = Some(val),
            _ if name.starts_with("x-apple-") => extra_headers.push((name, val)),
            _ => {}
        }
    }

    let mut body_bytes = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body_bytes);

    let mut builder = client
        .request(method, &target_url)
        .header("Origin", "https://music.apple.com")
        .header("Referer", "https://music.apple.com/");

    if let Some(auth) = incoming_auth.filter(|s| !s.trim().is_empty()) {
        builder = builder.header("Authorization", auth);
    } else if !dev_tok.trim().is_empty() {
        builder = builder.bearer_auth(dev_tok.trim());
    }

    let effective_user_tok = incoming_user_tok
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            if !user_tok.trim().is_empty() {
                Some(user_tok.trim().to_string())
            } else {
                None
            }
        });
    if let Some(ut) = effective_user_tok {
        builder = builder
            .header("Media-User-Token", &ut)
            .header("x-apple-music-user-token", &ut);
    }

    if let Some(ct) = incoming_content_type {
        builder = builder.header("Content-Type", ct);
    }
    if let Some(acc) = incoming_accept {
        builder = builder.header("Accept", acc);
    }
    for (k, v) in extra_headers {
        builder = builder.header(k, v);
    }
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes);
    }

    match builder.send() {
        Ok(upstream) => {
            let status = upstream.status().as_u16();
            dbg_log(&format!("[amp-proxy] {status} {}", target_url.chars().take(120).collect::<String>()));
            if is_storefront_req && !(200..300).contains(&status) {
                let json = storefront_fallback_json(&sf);
                let mut resp = tiny_http::Response::from_string(json).with_status_code(200);
                resp.add_header(
                    tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/json"[..],
                    )
                    .unwrap(),
                );
                for h in cors_headers() {
                    resp.add_header(h);
                }
                let _ = req.respond(resp);
                return;
            }

            let content_type = upstream
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let bytes = upstream.bytes().map(|b| b.to_vec()).unwrap_or_default();

            let mut resp = tiny_http::Response::from_data(bytes).with_status_code(status);
            if let Ok(ct_header) =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
            {
                resp.add_header(ct_header);
            }
            for h in cors_headers() {
                resp.add_header(h);
            }
            let _ = req.respond(resp);
        }
        Err(err) => {
            if is_storefront_req {
                let json = storefront_fallback_json(&sf);
                let mut resp = tiny_http::Response::from_string(json).with_status_code(200);
                resp.add_header(
                    tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/json"[..],
                    )
                    .unwrap(),
                );
                for h in cors_headers() {
                    resp.add_header(h);
                }
                let _ = req.respond(resp);
                return;
            }
            let err_json = format!(r#"{{"error":"Proxy request failed: {}"}}"#, err);
            let mut resp = tiny_http::Response::from_string(err_json).with_status_code(502);
            for h in cors_headers() {
                resp.add_header(h);
            }
            let _ = req.respond(resp);
        }
    }
}

#[cfg(test)]
pub fn render_bridge_html(developer_token: &str) -> String {
    render_bridge_html_with_context(developer_token, "us", "")
}

pub fn render_bridge_html_with_context(
    developer_token: &str,
    storefront: &str,
    existing_user_token: &str,
) -> String {
    let escaped_token = developer_token
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "");
    let escaped_sf = if storefront.trim().is_empty() {
        "us".to_string()
    } else {
        storefront
            .trim()
            .replace('\\', "")
            .replace('\'', "")
            .to_lowercase()
    };
    let escaped_user_token = existing_user_token
        .trim()
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "");

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Apple Music — gpui-webview MusicKit JS v3 Player & OAuth Bridge</title>
  <script>
    // Pre-configure localStorage & proxy interceptors BEFORE musickit.js loads!
    const devToken = '{escaped_token}';
    const configuredStorefront = '{escaped_sf}';
    let activeUserToken = '{escaped_user_token}';
    let capturedPopupToken = '';

    const seedMusicUserTokenInStorage = (token) => {{
      if (!token || token.length < 10) return;
      activeUserToken = token.trim();
      try {{
        localStorage.setItem('music.ampwebplay.media-user-token', activeUserToken);
        localStorage.setItem('music.AMPWebPlay.media-user-token', activeUserToken);
        localStorage.setItem('music.applemusicgpui.media-user-token', activeUserToken);
        // Also seed under JWT iss / kid if present
        const parts = devToken.split('.');
        if (parts.length >= 2) {{
          const b64 = parts[1].replace(/-/g, '+').replace(/_/g, '/');
          const payload = JSON.parse(atob(b64));
          if (payload && payload.iss) {{
            localStorage.setItem('music.' + payload.iss + '.media-user-token', activeUserToken);
            localStorage.setItem('music.' + String(payload.iss).toLowerCase() + '.media-user-token', activeUserToken);
          }}
        }}
        for (let i = 0; i < localStorage.length; i++) {{
          const k = localStorage.key(i);
          if (k && k.includes('media-user-token')) {{
            localStorage.setItem(k, activeUserToken);
          }}
        }}
      }} catch (_) {{}}
    }};

    seedMusicUserTokenInStorage(activeUserToken);

    // Ensure MusicKit's internal token lookup always finds activeUserToken
    try {{
      const origGetItem = Storage.prototype.getItem;
      Storage.prototype.getItem = function(key) {{
        const val = origGetItem.call(this, key);
        if (!val && key && (key.includes('media-user-token') || key.includes('musicUserToken') || key.includes('user-token'))) {{
          if (activeUserToken && activeUserToken.length > 10) {{
            return activeUserToken;
          }}
        }}
        return val;
      }};
    }} catch (_) {{}}
  </script>
  <script src="https://js-cdn.music.apple.com/musickit/v3/musickit.js" async></script>
  <style>
    :root {{
      color-scheme: dark;
    }}
    body {{
      margin: 0;
      padding: 20px;
      font-family: -apple-system, BlinkMacSystemFont, "SF Pro Display", "Segoe UI", sans-serif;
      background: radial-gradient(circle at top right, #2b1222, #121216 70%);
      color: #f5f5f7;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      min-height: calc(100vh - 40px);
      text-align: center;
    }}
    .card {{
      background: rgba(255, 255, 255, 0.06);
      border: 1px solid rgba(255, 255, 255, 0.12);
      border-radius: 16px;
      padding: 20px 24px;
      max-width: 440px;
      width: 100%;
      box-sizing: border-box;
      box-shadow: 0 20px 40px rgba(0,0,0,0.45);
    }}
    h2 {{
      margin: 0 0 6px 0;
      font-size: 18px;
      font-weight: 600;
    }}
    p {{
      margin: 0 0 14px 0;
      font-size: 12px;
      line-height: 1.45;
      color: #a1a1aa;
    }}
    .status {{
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 12px;
      padding: 8px 12px;
      border-radius: 8px;
      background: rgba(0, 0, 0, 0.35);
      color: #86efac;
      margin-bottom: 14px;
      word-break: break-word;
    }}
    button {{
      background: #fa2d48;
      color: white;
      border: none;
      border-radius: 10px;
      padding: 10px 18px;
      font-size: 13px;
      font-weight: 600;
      cursor: pointer;
      width: 100%;
      margin-bottom: 8px;
      transition: opacity 0.15s;
    }}
    button:hover {{
      opacity: 0.92;
    }}
    button.secondary {{
      background: rgba(255, 255, 255, 0.1);
      color: #e4e4e7;
    }}
  </style>
</head>
<body>
  <div class="card">
    <h2>Apple Music gpui-webview DRM Player & OAuth</h2>
    <p>Hosts MusicKit JS v3 inside native WebKit EME CDM to stream full-length Apple Music subscription tracks and handle Apple ID authorization.</p>
    <div id="status" class="status">Initializing MusicKit JS v3...</div>
    <button id="auth-btn" onclick="window.gpuiBridge && window.gpuiBridge.authorize()">
      Sign In with Apple ID (MusicKit.authorize)
    </button>
    <button class="secondary" onclick="window.gpuiBridge && window.gpuiBridge.simulateToken()">
      Emit Demo Media-User-Token
    </button>
  </div>

  <script>
    const statusEl = document.getElementById('status');
    const setStatus = (msg) => {{
      if (statusEl) statusEl.textContent = msg;
    }};

    const sendToRust = (type, payload = {{}}) => {{
      if (window.ipc && window.ipc.postMessage) {{
        window.ipc.postMessage(JSON.stringify({{ type, payload }}));
      }}
    }};

    window.addEventListener('error', (e) => {{
      sendToRust('error', {{ message: 'JS error: ' + (e.message || e.error) }});
    }});
    window.addEventListener('unhandledrejection', (e) => {{
      const r = e && e.reason;
      sendToRust('error', {{ message: 'JS rejection: ' + ((r && (r.message || r.name)) || String(r)) }});
    }});
    // Forward failed proxied requests so DRM/license failures are visible
    const _origFetch2 = window.fetch;
    window.fetch = async (...args) => {{
      const resp = await _origFetch2(...args);
      try {{
        const u = typeof args[0] === 'string' ? args[0] : (args[0] && args[0].url) || '';
        if (u.includes('/amp-proxy') && resp && !resp.ok) {{
          sendToRust('error', {{ message: 'HTTP ' + resp.status + ' for ' + decodeURIComponent(u.split('url=')[1] || u).slice(0, 110) }});
        }}
      }} catch (_) {{}}
      return resp;
    }};

    let musickitConfigured = false;
    let resolvedStorefront = configuredStorefront || 'us';
    let accountStorefrontFetched = false;
    let isSwitchingTrack = false;
    let pendingSongRequest = null;

    const resolveAccountStorefront = async (force = false) => {{
      const ut = findStoredMusicUserToken();
      if (!devToken || !ut) return resolvedStorefront;
      if (accountStorefrontFetched && !force) return resolvedStorefront;
      try {{
        const r = await fetch('https://amp-api.music.apple.com/v1/me/storefront');
        if (r && r.ok) {{
          const j = await r.json();
          const real = j && j.data && j.data[0] && j.data[0].id;
          if (real && typeof real === 'string') {{
            const sf = real.trim().toLowerCase();
            if (sf) {{
              resolvedStorefront = sf;
              accountStorefrontFetched = true;
              sendToRust('storefront_detected', {{ storefront: sf }});
              try {{
                if (typeof MusicKit !== 'undefined' && musickitConfigured) {{
                  MusicKit.getInstance().storefrontId = sf;
                }}
              }} catch (_) {{}}
              return sf;
            }}
          }}
        }}
      }} catch (_) {{}}
      return resolvedStorefront;
    }};

    const findStoredMusicUserToken = () => {{
      if (capturedPopupToken && capturedPopupToken.length > 10) return capturedPopupToken;
      if (activeUserToken && activeUserToken.length > 10) return activeUserToken;
      try {{
        if (typeof MusicKit !== 'undefined') {{
          const inst = MusicKit.getInstance();
          if (inst && inst.musicUserToken && inst.musicUserToken.length > 10) {{
            return inst.musicUserToken;
          }}
        }}
      }} catch (_) {{}}
      try {{
        for (let i = 0; i < localStorage.length; i++) {{
          const k = localStorage.key(i);
          if (k && (k.includes('media-user-token') || k.includes('musicUserToken'))) {{
            const v = localStorage.getItem(k);
            if (v && v.length > 10) return v.replace(/^"|"$/g, '');
          }}
        }}
      }} catch (_) {{}}
      return '';
    }};

    const shouldProxyUrl = (u) => {{
      if (!u || typeof u !== 'string') return false;
      return u.startsWith('https://amp-api.music.apple.com/')
        || u.startsWith('https://api.music.apple.com/')
        || u.startsWith('https://play.music.apple.com/')
        || u.startsWith('https://play.itunes.apple.com/')
        || u.startsWith('https://buy.itunes.apple.com/');
    }};

    // 1. Intercept fetch() so AMPWebPlay / MusicKit requests route through our Rust /amp-proxy
    //    with Origin: https://music.apple.com, Authorization, and Media-User-Token!
    const origFetch = window.fetch.bind(window);
    window.fetch = async (input, init) => {{
      const url = typeof input === 'string' ? input : (input && input.url ? input.url : '');
      const makeStorefrontResponse = () => new Response(
        JSON.stringify({{
          data: [{{
            id: configuredStorefront || 'tw',
            type: 'storefronts',
            attributes: {{
              defaultLanguageTag: 'zh-Hant-TW',
              name: (configuredStorefront || 'tw').toUpperCase(),
              supportedLanguageTags: ['zh-Hant-TW', 'en-US']
            }}
          }}]
        }}),
        {{ status: 200, headers: {{ 'Content-Type': 'application/json' }} }}
      );

      if (shouldProxyUrl(url)) {{
        try {{
          const method = (init && init.method) || (input instanceof Request ? input.method : 'GET');
          const mergedHeaders = new Headers();
          if (input instanceof Request && input.headers) {{
            input.headers.forEach((v, k) => mergedHeaders.set(k, v));
          }}
          if (init && init.headers) {{
            new Headers(init.headers).forEach((v, k) => mergedHeaders.set(k, v));
          }}
          if (devToken && !mergedHeaders.has('Authorization')) {{
            mergedHeaders.set('Authorization', 'Bearer ' + devToken);
          }}
          const ut = findStoredMusicUserToken();
          if (ut && !mergedHeaders.has('Media-User-Token')) {{
            mergedHeaders.set('Media-User-Token', ut);
            mergedHeaders.set('x-apple-music-user-token', ut);
          }}

          let body = init ? init.body : undefined;
          if (body === undefined && input instanceof Request && method !== 'GET' && method !== 'HEAD') {{
            try {{
              body = await input.clone().arrayBuffer();
            }} catch (_) {{}}
          }}

          const proxiedUrl = '/amp-proxy?url=' + encodeURIComponent(url);
          const resp = await origFetch(proxiedUrl, {{
            method,
            headers: mergedHeaders,
            body
          }});
          if (url.includes('/v1/me/storefront') && (!resp || !resp.ok)) {{
            return makeStorefrontResponse();
          }}
          return resp;
        }} catch (err) {{
          if (url.includes('/v1/me/storefront')) {{
            return makeStorefrontResponse();
          }}
          throw err;
        }}
      }}

      if (url.includes('/v1/me/storefront')) {{
        return makeStorefrontResponse();
      }}
      return origFetch(input, init);
    }};

    // Also intercept XMLHttpRequest for any EME / WebPlayer calls that use XHR
    try {{
      const origXhrOpen = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function(method, url, ...rest) {{
        if (typeof url === 'string' && shouldProxyUrl(url)) {{
          const proxiedUrl = '/amp-proxy?url=' + encodeURIComponent(url);
          return origXhrOpen.call(this, method, proxiedUrl, ...rest);
        }}
        return origXhrOpen.call(this, method, url, ...rest);
      }};
    }} catch (_) {{}}

    // 2. Capture Media-User-Token directly from Apple's authorize.music.apple.com popup postMessage
    window.addEventListener('message', (event) => {{
      try {{
        const data = typeof event.data === 'string' ? JSON.parse(event.data) : event.data;
        if (!data) return;
        const tok = data.musicUserToken
          || data.userToken
          || (data.payload && (data.payload.musicUserToken || data.payload.userToken))
          || (data.details && data.details.musicUserToken);
        if (tok && typeof tok === 'string' && tok.length > 10) {{
          capturedPopupToken = tok.trim();
          seedMusicUserTokenInStorage(capturedPopupToken);
          try {{
            if (typeof MusicKit !== 'undefined') {{
              MusicKit.getInstance().musicUserToken = capturedPopupToken;
            }}
          }} catch (_) {{}}
          setStatus('Captured Media-User-Token! Full-song DRM playback enabled.');
          sendToRust('authorized', {{ userToken: capturedPopupToken }});
        }}
      }} catch (_) {{}}
    }});

    const getEffectiveDuration = (music) => {{
      if (!music) return 0;
      const d = Number(music.currentPlaybackDuration || 0);
      if (d > 1) return d;
      const item = music.nowPlayingItem;
      if (item) {{
        const pd = Number(
          item.playbackDuration
          || (item.attributes && item.attributes.durationInMillis)
          || 0
        );
        if (pd > 1000) return pd / 1000.0;
        if (pd > 1) return pd;
      }}
      return 0;
    }};

    const waitForAdvance = async (music, ms) => {{
      const start = Date.now();
      let t0 = null;
      while (Date.now() - start < ms) {{
        try {{
          const t = Number(music.currentPlaybackTime || 0);
          if (music.nowPlayingItem && music.isPlaying) {{
            if (t0 === null) t0 = t;
            if (t - t0 > 0.4 || t > 0.6) return true;
          }}
        }} catch (_) {{}}
        await new Promise((r) => setTimeout(r, 150));
      }}
      return false;
    }};

    window.gpuiBridge = {{
      authorize: async () => {{
        try {{
          if (typeof MusicKit === 'undefined') {{
            setStatus('MusicKit JS CDN not loaded yet or blocked.');
            sendToRust('error', {{ message: 'MusicKit JS not loaded yet.' }});
            return;
          }}
          const music = MusicKit.getInstance();
          setStatus('Opening Apple ID OAuth popup...');
          const userToken = await music.authorize();
          const finalToken = userToken || findStoredMusicUserToken();
          if (finalToken) {{
            seedMusicUserTokenInStorage(finalToken);
            setStatus('Authorized! Full-song DRM playback active in gpui-webview.');
            sendToRust('authorized', {{ userToken: finalToken }});
          }}
        }} catch (err) {{
          const recovered = findStoredMusicUserToken();
          if (recovered) {{
            seedMusicUserTokenInStorage(recovered);
            setStatus('Authorized (Storefront "' + configuredStorefront + '" applied)! Full-song DRM playback active.');
            sendToRust('authorized', {{ userToken: recovered }});
            return;
          }}
          setStatus('OAuth Error: ' + err.toString());
          sendToRust('error', {{ message: err.toString() }});
        }}
      }},
      simulateToken: () => {{
        const fakeToken = 'Mut-GPUI-Ephemeral-' + Math.random().toString(36).substring(2, 12) + '-' + Date.now();
        seedMusicUserTokenInStorage(fakeToken);
        setStatus('Emitted Demo Media-User-Token to GPUI.');
        sendToRust('authorized', {{ userToken: fakeToken.trim() }});
      }},
      playSong: async (songId, fallbackUrl, libraryId) => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) {{
            pendingSongRequest = {{ songId: String(songId), fallbackUrl: fallbackUrl || '', libraryId: libraryId || '' }};
            setStatus('Waiting for MusicKit JS v3 to finish configuring before playing ' + songId + '...');
            return;
          }}
          const music = MusicKit.getInstance();
          const ut = findStoredMusicUserToken();
          if (ut) {{
            seedMusicUserTokenInStorage(ut);
            try {{ music.musicUserToken = ut; }} catch (_) {{}}
          }}
          const sid = String(songId).trim();
          const lid = String(libraryId || '').trim();
          setStatus('Loading full track (' + sid + ') via MusicKit JS v3...');

          // Stop whatever is playing cleanly without emitting stale stopped states to Rust
          isSwitchingTrack = true;
          try {{ await music.pause(); }} catch (_) {{}}
          try {{ await music.stop(); }} catch (_) {{}}
          isSwitchingTrack = false;

          // Always try the account's real storefront first, then region fallbacks if needed
          const storefronts = [];
          const real = await resolveAccountStorefront();
          if (real) storefronts.push(real);
          if (configuredStorefront && !storefronts.includes(configuredStorefront)) storefronts.push(configuredStorefront);
          ['us', 'tw', 'jp', 'gb'].forEach((s) => {{ if (!storefronts.includes(s)) storefronts.push(s); }});

          const attempts = [];
          if (sid.startsWith('i.')) {{
            attempts.push({{ librarySong: sid, startPlaying: true }});
          }} else {{
            storefronts.slice(0, 3).forEach((sf) => attempts.push({{ song: sid, startPlaying: true, sf }}));
            // Library ID only as a last resort (MusicKit's library-songs request often returns HTTP 400)
            if (lid) attempts.push({{ librarySong: lid, startPlaying: true }});
          }}

          let lastErr = null;
          let started = false;
          for (const att of attempts) {{
            const attemptStart = Date.now();
            try {{
              const {{ sf, ...queueArg }} = att;
              if (sf && music.storefrontId !== sf) {{
                try {{ music.storefrontId = sf; }} catch (_) {{}}
              }}
              await music.setQueue(queueArg);
              // startPlaying:true already plays; calling play() again makes MusicKit throw and pause.
              if (!music.isPlaying) {{ try {{ await music.play(); }} catch (_) {{}} }}
              started = true;
              sendToRust('error', {{ message: 'Queue OK via ' + JSON.stringify(att).slice(0, 80) }});
              break;
            }} catch (e) {{
              const emsg = String((e && (e.message || e.name)) || e);
              if (emsg.includes('CONTENT_EQUIVALENT')) {{
                const elapsed = Date.now() - attemptStart;
                if (elapsed > 700 || music.isPlaying) {{
                  const ok = await waitForAdvance(music, 3500);
                  if (ok) {{
                    started = true;
                    sendToRust('error', {{ message: 'Queue OK (content-equivalent, audio advancing)' }});
                    setTimeout(async () => {{
                      try {{ if (!music.isPlaying && music.nowPlayingItem) await music.play(); }} catch (_) {{}}
                    }}, 1800);
                    break;
                  }}
                }}
              }}
              lastErr = e;
              sendToRust('error', {{ message: 'Attempt ' + JSON.stringify(att).slice(0, 60) + ' failed: ' + (e && (e.message || e.name) || e) }});
            }}
          }}
          if (!started) {{
            // Diagnostics: log what Apple says about this item so we can see why it is not playable
            const diag = async (label, url) => {{
              try {{
                const r = await fetch(url);
                const t = await r.text();
                sendToRust('error', {{ message: 'DIAG ' + label + ' HTTP ' + r.status + ': ' + t.slice(0, 900) }});
              }} catch (e) {{
                sendToRust('error', {{ message: 'DIAG ' + label + ' failed: ' + e }});
              }}
            }};
            await diag('catalog-' + (storefronts[0] || 'tw'), 'https://amp-api.music.apple.com/v1/catalog/' + (storefronts[0] || 'tw') + '/songs/' + sid + '?extend=extendedAssetUrls');
            if (lid) await diag('library', 'https://amp-api.music.apple.com/v1/me/library/songs/' + lid + '?include=catalog');
            throw (lastErr || new Error('all queue attempts failed'));
          }}

          const dur = getEffectiveDuration(music);
          setStatus('Playing full track (' + sid + ')' + (dur > 0 ? ' [' + Math.round(dur) + 's]' : '') + ' via gpui-webview');
          sendToRust('playback_state', {{ state: 2, isPlaying: true }});
          if (dur > 0) {{
            sendToRust('playback_time', {{
              current: Number(music.currentPlaybackTime || 0),
              duration: dur
            }});
          }}
        }} catch (err) {{
          const msg = err ? err.toString() : 'Unknown MusicKit play error';
          setStatus('MusicKit playSong notice: ' + msg);
          if (fallbackUrl && fallbackUrl.length > 0) {{
            sendToRust('fallback_preview', {{ url: fallbackUrl, reason: msg }});
          }} else {{
            sendToRust('error', {{ message: 'MusicKit playSong error: ' + msg }});
          }}
        }}
      }},
      play: async () => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) return;
          const music = MusicKit.getInstance();
          await music.play();
          sendToRust('playback_state', {{ state: 2, isPlaying: true }});
        }} catch (_) {{}}
      }},
      pause: () => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) return;
          const music = MusicKit.getInstance();
          music.pause();
          sendToRust('playback_state', {{ state: 3, isPlaying: false }});
        }} catch (_) {{}}
      }},
      togglePlayPause: async () => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) return;
          const music = MusicKit.getInstance();
          if (music.isPlaying) {{
            music.pause();
            sendToRust('playback_state', {{ state: 3, isPlaying: false }});
          }} else {{
            await music.play();
            sendToRust('playback_state', {{ state: 2, isPlaying: true }});
          }}
        }} catch (_) {{}}
      }},
      stop: () => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) return;
          const music = MusicKit.getInstance();
          music.stop();
          sendToRust('playback_state', {{ state: 4, isPlaying: false }});
        }} catch (_) {{}}
      }},
      seekTo: async (time) => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) return;
          const music = MusicKit.getInstance();
          await music.seekToTime(Number(time) || 0);
        }} catch (_) {{}}
      }},
      setVolume: (vol) => {{
        try {{
          if (typeof MusicKit === 'undefined' || !musickitConfigured) return;
          const music = MusicKit.getInstance();
          music.volume = Math.max(0, Math.min(1, Number(vol)));
        }} catch (_) {{}}
      }}
    }};

    document.addEventListener('musickitloaded', async () => {{
      try {{
        if (!devToken || devToken.length < 10) {{
          setStatus('Ready (No Developer JWT configured — configure one in GPUI below).');
          sendToRust('ready');
          return;
        }}
        seedMusicUserTokenInStorage(activeUserToken);
        if (findStoredMusicUserToken()) {{
          await resolveAccountStorefront();
        }}
        await MusicKit.configure({{
          developerToken: devToken,
          storefrontId: resolvedStorefront || 'us',
          app: {{ name: 'AppleMusicGPUI', build: '1.0.0' }}
        }});

        const music = MusicKit.getInstance();
        const storedUt = findStoredMusicUserToken();
        if (storedUt) {{
          try {{ music.musicUserToken = storedUt; }} catch (_) {{}}
        }}
        musickitConfigured = true;

        music.addEventListener('playbackStateDidChange', (e) => {{
          if (isSwitchingTrack) return;
          const st = e && typeof e.state === 'number' ? e.state : 0;
          sendToRust('playback_state', {{
            state: st,
            isPlaying: Boolean(music.isPlaying || st === 2)
          }});
          const dur = getEffectiveDuration(music);
          if (dur > 0) {{
            sendToRust('playback_time', {{
              current: Number(music.currentPlaybackTime || 0),
              duration: dur
            }});
          }}
        }});

        music.addEventListener('playbackTimeDidChange', (e) => {{
          const cur = Number((e && e.currentPlaybackTime) ?? music.currentPlaybackTime ?? 0);
          const dur = Number((e && e.currentPlaybackDuration) || getEffectiveDuration(music) || 0);
          sendToRust('playback_time', {{
            current: cur,
            duration: dur
          }});
        }});

        // Surface real playback/DRM errors instead of silently stopping
        ['mediaPlaybackError', 'playbackError', 'mediaItemStateDidChange'].forEach((evName) => {{
          try {{
            music.addEventListener(evName, (e) => {{
              if (evName === 'mediaItemStateDidChange') return;
              const msg = (e && (e.message || (e.error && e.error.message) || e.name || JSON.stringify(e))) || 'unknown';
              setStatus('MusicKit ' + evName + ': ' + msg);
              sendToRust('error', {{ message: 'MusicKit ' + evName + ': ' + msg }});
            }});
          }} catch (_) {{}}
        }});

        music.addEventListener('nowPlayingItemDidChange', () => {{
          const dur = getEffectiveDuration(music);
          if (dur > 0) {{
            sendToRust('playback_time', {{
              current: Number(music.currentPlaybackTime || 0),
              duration: dur
            }});
          }}
        }});

        // Periodic telemetry sync while playing
        setInterval(() => {{
          try {{
            if (music && music.isPlaying) {{
              const cur = Number(music.currentPlaybackTime || 0);
              const dur = getEffectiveDuration(music);
              if (dur > 0 || cur > 0) {{
                sendToRust('playback_time', {{ current: cur, duration: dur }});
              }}
            }}
          }} catch (_) {{}}
        }}, 250);

        if (storedUt) {{
          setStatus('MusicKit v3 Configured & Authorized (Storefront: ' + (resolvedStorefront || 'us').toUpperCase() + ') — Ready for Full-Song Playback.');
        }} else {{
          setStatus('MusicKit v3 Configured — Click Sign In above or paste Media-User-Token below.');
        }}
        sendToRust('ready');

        if (pendingSongRequest) {{
          const req = pendingSongRequest;
          pendingSongRequest = null;
          window.gpuiBridge.playSong(req.songId, req.fallbackUrl, req.libraryId);
        }}
      }} catch (err) {{
        setStatus('MusicKit Configure Error: ' + err.toString());
        sendToRust('error', {{ message: err.toString() }});
      }}
    }});
  </script>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_bridge_html_for_debug() {
        let html = render_bridge_html_with_context("a.b.c", "tw", "tok1234567890");
        let _ = std::fs::write("/tmp/bridge_dump.html", html);
    }

    #[test]
    fn test_bridge_event_deserialization() {
        let ready: BridgeEvent =
            serde_json::from_str(r#"{"type":"ready","payload":{}}"#).unwrap();
        assert!(matches!(ready, BridgeEvent::Ready {}));

        let auth: BridgeEvent = serde_json::from_str(
            r#"{"type":"authorized","payload":{"userToken":"Mut-Test-123"}}"#,
        )
        .unwrap();
        match auth {
            BridgeEvent::Authorized { user_token } => assert_eq!(user_token, "Mut-Test-123"),
            _ => panic!("Expected Authorized event"),
        }

        let state_ev: BridgeEvent = serde_json::from_str(
            r#"{"type":"playback_state","payload":{"state":2,"isPlaying":true}}"#,
        )
        .unwrap();
        match state_ev {
            BridgeEvent::PlaybackState { state, is_playing } => {
                assert_eq!(state, 2);
                assert!(is_playing);
            }
            _ => panic!("Expected PlaybackState event"),
        }

        let sf_ev: BridgeEvent = serde_json::from_str(
            r#"{"type":"storefront_detected","payload":{"storefront":"tw"}}"#,
        )
        .unwrap();
        match sf_ev {
            BridgeEvent::StorefrontDetected { storefront } => assert_eq!(storefront, "tw"),
            _ => panic!("Expected StorefrontDetected event"),
        }

        assert_eq!(
            percent_decode("https%3A%2F%2Famp-api.music.apple.com%2Fv1%2Fcatalog%2Ftw%2Fsongs%2F123"),
            "https://amp-api.music.apple.com/v1/catalog/tw/songs/123"
        );

        let html = render_bridge_html("eyJhbGciOiJFUzI1NiJ9.test");
        assert!(html.contains("musickit.js"));
        assert!(html.contains("eyJhbGciOiJFUzI1NiJ9.test"));
        assert!(html.contains("window.ipc.postMessage"));
        assert!(html.contains("/amp-proxy?url="));
    }
}
