//! Minimal client for the Reolink HTTP JSON API (`/cgi-bin/api.cgi`).
//!
//! Uses token login (`cmd=Login`) and transparently re-authenticates when the
//! camera reports an expired session (`rspCode -6`).

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::CameraConfig;

/// Re-login this long before the camera-side lease expires.
const LEASE_MARGIN: Duration = Duration::from_secs(60);
const RSP_LOGIN_REQUIRED: i64 = -6;

#[derive(Debug)]
pub struct ApiError {
    pub cmd: String,
    pub rsp_code: i64,
    pub detail: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {} (rspCode {})", self.cmd, self.detail, self.rsp_code)
    }
}

impl std::error::Error for ApiError {}

/// AI detection flags reported by `GetAiState`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AiState {
    pub supported: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
}

/// A clip on the camera's SD card. Times are camera-local (h, m, s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub name: String,
    pub start: (u32, u32, u32),
    pub end: (u32, u32, u32),
    pub size: u64,
}

impl Recording {
    pub fn duration_secs(&self) -> u32 {
        let secs = |(h, m, s): (u32, u32, u32)| h * 3600 + m * 60 + s;
        secs(self.end).saturating_sub(secs(self.start))
    }

    /// File name without the `Mp4Record/<date>/` prefix.
    pub fn file_name(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub model: String,
    pub firmware: String,
    /// Name set on the camera itself.
    pub name: String,
}

struct Token {
    name: String,
    expires: Instant,
}

pub struct Client {
    http: reqwest::blocking::Client,
    endpoint: String,
    username: String,
    password: String,
    channel: u8,
    token: Option<Token>,
}

impl Client {
    pub fn new(cam: &CameraConfig) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            // Reolink cameras ship self-signed certificates on the LAN.
            .danger_accept_invalid_certs(true)
            // Redirects are handled by `send` (HTTP→HTTPS upgrade); following them
            // blindly would turn the POST into a GET on `/` and lose the query.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let scheme = if cam.use_https { "https" } else { "http" };
        Ok(Self {
            http,
            endpoint: format!("{scheme}://{}/cgi-bin/api.cgi", cam.host.trim()),
            username: cam.username.clone(),
            password: cam.password.clone(),
            channel: cam.channel,
            token: None,
        })
    }

    pub fn login(&mut self) -> Result<()> {
        let body = json!([{
            "cmd": "Login",
            "param": { "User": { "Version": "0", "userName": self.username, "password": self.password } }
        }]);
        let value = self.post("Login", None, &body)?;
        let name = value["Token"]["name"]
            .as_str()
            .context("login response has no token")?
            .to_owned();
        let lease = Duration::from_secs(value["Token"]["leaseTime"].as_u64().unwrap_or(3600));
        let lifetime = lease.saturating_sub(LEASE_MARGIN).max(Duration::from_secs(30));
        self.token = Some(Token { name, expires: Instant::now() + lifetime });
        log::info!("logged in to {}", self.endpoint);
        Ok(())
    }

    /// Releases the session slot on the camera (they only allow a few).
    pub fn logout(&mut self) {
        if let Some(token) = self.token.take() {
            let body = json!([{ "cmd": "Logout", "param": {} }]);
            if let Err(e) = self.post("Logout", Some(&token.name), &body) {
                log::debug!("logout: {e:#}");
            }
        }
    }

    /// Model, firmware and device name (`GetDevInfo`).
    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        let value = self.call("GetDevInfo", json!({}))?;
        let info = &value["DevInfo"];
        let text = |key: &str| info[key].as_str().unwrap_or_default().to_owned();
        Ok(DeviceInfo { model: text("model"), firmware: text("firmVer"), name: text("name") })
    }

    /// `true` while the camera's motion detector is triggered.
    pub fn motion_state(&mut self) -> Result<bool> {
        let value = self.call("GetMdState", json!({ "channel": self.channel }))?;
        Ok(value["state"].as_i64() == Some(1))
    }

    pub fn ai_state(&mut self) -> Result<AiState> {
        let value = self.call("GetAiState", json!({ "channel": self.channel }))?;
        let flag = |key: &str| value[key]["alarm_state"].as_i64() == Some(1);
        let supported = |key: &str| value[key]["support"].as_i64() == Some(1);
        Ok(AiState {
            supported: supported("people") || supported("vehicle") || supported("dog_cat"),
            person: flag("people"),
            vehicle: flag("vehicle"),
            animal: flag("dog_cat"),
        })
    }

    /// Spotlight (white LED) state, or `None` if the camera has no spotlight.
    pub fn spotlight(&mut self) -> Result<Option<bool>> {
        match self.call("GetWhiteLed", json!({ "channel": self.channel })) {
            Ok(v) => Ok(v["WhiteLed"]["state"].as_i64().map(|s| s == 1)),
            Err(e) if e.downcast_ref::<ApiError>().is_some() => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn set_spotlight(&mut self, on: bool) -> Result<()> {
        let param = json!({ "WhiteLed": { "channel": self.channel, "state": i32::from(on) } });
        self.call("SetWhiteLed", param).map(drop)
    }

    /// Days (1-based) of `month` that have recordings.
    pub fn recording_days(&mut self, year: i32, month: u32, stream: &str) -> Result<Vec<u32>> {
        let last = days_in_month(year, month);
        let value = self.search(stream, true, (year, month, 1, 0, 0, 0), (year, month, last, 23, 59, 59))?;
        let days = value["SearchResult"]["Status"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["year"].as_i64() == Some(year.into()) && s["mon"].as_i64() == Some(month.into()))
            .filter_map(|s| s["table"].as_str())
            .flat_map(|table| table.chars().enumerate().filter(|(_, c)| *c == '1').map(|(i, _)| i as u32 + 1))
            .collect();
        Ok(days)
    }

    /// Clips recorded on the given day, newest first.
    pub fn recordings(&mut self, year: i32, month: u32, day: u32, stream: &str) -> Result<Vec<Recording>> {
        let value = self.search(stream, false, (year, month, day, 0, 0, 0), (year, month, day, 23, 59, 59))?;
        let mut clips: Vec<Recording> = value["SearchResult"]["File"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_recording)
            .collect();
        clips.sort_by(|a, b| b.start.cmp(&a.start));
        Ok(clips)
    }

    /// HTTPS URL of a clip. It supports range requests (seeking) and stays
    /// valid while this client's session token does.
    pub fn download_url(&mut self, name: &str) -> Result<String> {
        let token = self.ensure_token()?;
        let file = name.rsplit('/').next().unwrap_or(name);
        let url = reqwest::Url::parse_with_params(
            &self.endpoint,
            &[("cmd", "Download"), ("source", name), ("output", file), ("token", token.as_str())],
        )?;
        Ok(url.into())
    }

    pub fn download(&mut self, name: &str) -> Result<Vec<u8>> {
        let url = self.download_url(name)?;
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(600))
            .send()?
            .error_for_status()?;
        Ok(resp.bytes()?.to_vec())
    }

    fn search(
        &mut self,
        stream: &str,
        only_status: bool,
        from: (i32, u32, u32, u32, u32, u32),
        to: (i32, u32, u32, u32, u32, u32),
    ) -> Result<Value> {
        let time = |(year, mon, day, hour, min, sec): (i32, u32, u32, u32, u32, u32)| {
            json!({ "year": year, "mon": mon, "day": day, "hour": hour, "min": min, "sec": sec })
        };
        self.call(
            "Search",
            json!({ "Search": {
                "channel": self.channel,
                "onlyStatus": i32::from(only_status),
                "streamType": stream,
                "StartTime": time(from),
                "EndTime": time(to),
            }}),
        )
    }

    /// Current frame as JPEG bytes.
    pub fn snapshot(&mut self) -> Result<Vec<u8>> {
        let token = self.ensure_token()?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().to_string();
        let channel = self.channel.to_string();
        let resp = self
            .http
            .get(&self.endpoint)
            .query(&[("cmd", "Snap"), ("channel", &channel), ("rs", &nonce), ("token", &token)])
            .send()?
            .error_for_status()?;
        let is_jpeg = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("image/"));
        if !is_jpeg {
            bail!("Snap did not return an image");
        }
        Ok(resp.bytes()?.to_vec())
    }

    fn ensure_token(&mut self) -> Result<String> {
        match &self.token {
            Some(t) if t.expires > Instant::now() => Ok(t.name.clone()),
            _ => {
                self.login()?;
                Ok(self.token.as_ref().expect("token set by login").name.clone())
            }
        }
    }

    fn call(&mut self, cmd: &str, param: Value) -> Result<Value> {
        let body = json!([{ "cmd": cmd, "action": 0, "param": param }]);
        let token = self.ensure_token()?;
        match self.post(cmd, Some(&token), &body) {
            Err(e) if e.downcast_ref::<ApiError>().is_some_and(|a| a.rsp_code == RSP_LOGIN_REQUIRED) => {
                log::info!("session expired, logging in again");
                self.token = None;
                let token = self.ensure_token()?;
                self.post(cmd, Some(&token), &body)
            }
            other => other,
        }
    }

    fn post(&mut self, cmd: &str, token: Option<&str>, body: &Value) -> Result<Value> {
        let send = |endpoint: &str| {
            let mut req = self.http.post(endpoint).query(&[("cmd", cmd)]);
            if let Some(token) = token {
                req = req.query(&[("token", token)]);
            }
            req.json(body).send().with_context(|| format!("{cmd}: request to camera failed"))
        };
        let mut resp = send(&self.endpoint)?;
        // Newer firmwares redirect plain HTTP to HTTPS: switch over for good.
        if resp.status().is_redirection() && self.endpoint.starts_with("http://") {
            let https = self.endpoint.replacen("http://", "https://", 1);
            log::info!("camera redirects to HTTPS; using {https}");
            self.endpoint = https;
            resp = send(&self.endpoint)?;
        }
        let resp: Value = resp
            .error_for_status()?
            .json()
            .with_context(|| format!("{cmd}: invalid JSON response"))?;
        parse_response(cmd, &resp)
    }
}

fn parse_recording(file: &Value) -> Option<Recording> {
    let hms = |t: &Value| -> Option<(u32, u32, u32)> {
        Some((t["hour"].as_u64()? as u32, t["min"].as_u64()? as u32, t["sec"].as_u64()? as u32))
    };
    Some(Recording {
        name: file["name"].as_str()?.to_owned(),
        start: hms(&file["StartTime"])?,
        end: hms(&file["EndTime"])?,
        // The camera sends the size as a string.
        size: file["size"].as_str().and_then(|s| s.parse().ok()).or(file["size"].as_u64()).unwrap_or(0),
    })
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse_response(cmd: &str, resp: &Value) -> Result<Value> {
    let entry = resp.get(0).with_context(|| format!("{cmd}: empty response"))?;
    if entry["code"].as_i64() == Some(0) {
        return Ok(entry["value"].clone());
    }
    Err(ApiError {
        cmd: cmd.to_owned(),
        rsp_code: entry["error"]["rspCode"].as_i64().unwrap_or(0),
        detail: entry["error"]["detail"].as_str().unwrap_or("unknown error").to_owned(),
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recording() {
        let f = json!({
            "StartTime": { "year": 2026, "mon": 10, "day": 14, "hour": 23, "min": 37, "sec": 13 },
            "EndTime":   { "year": 2026, "mon": 10, "day": 14, "hour": 23, "min": 37, "sec": 59 },
            "name": "Mp4Record/2026-10-14/RecS05_DST20261014_233712_233759_0_3914C0800_18A088.mp4",
            "size": "1613960", "type": "sub"
        });
        let r = parse_recording(&f).unwrap();
        assert_eq!(r.start, (23, 37, 13));
        assert_eq!(r.duration_secs(), 46);
        assert_eq!(r.size, 1_613_960);
        assert!(r.file_name().starts_with("RecS05_"));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
    }

    #[test]
    fn parses_success_and_error() {
        let ok = json!([{ "cmd": "GetMdState", "code": 0, "value": { "state": 1 } }]);
        assert_eq!(parse_response("GetMdState", &ok).unwrap()["state"], 1);

        let err = json!([{ "cmd": "GetMdState", "code": 1,
                           "error": { "detail": "please login first", "rspCode": -6 } }]);
        let e = parse_response("GetMdState", &err).unwrap_err();
        assert_eq!(e.downcast_ref::<ApiError>().unwrap().rsp_code, RSP_LOGIN_REQUIRED);
    }
}
