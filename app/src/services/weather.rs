//! Weather service — Open-Meteo API (free, no key required).
//!
//! Location: the car's latest GPS fix, else the optional `--lat/--lon`
//! fallback. With neither, nothing is fetched until the first fix.

use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::gps::GpsService;

#[derive(Debug, Clone, Default, Serialize)]
pub struct WeatherData {
    pub temperature: f64,
    pub feels_like: f64,
    pub condition: String,
    pub icon: String,
    pub humidity: u32,
    pub wind_speed_kmh: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForecastEntry {
    pub time: String,
    pub temp: f64,
    pub icon: String,
    /// Chance of precipitation for that hour, in percent.
    pub precip_prob: Option<u32>,
}

/// One day of the 7-day forecast.
#[derive(Debug, Clone, Serialize)]
pub struct DailyEntry {
    pub date: String,
    pub max: f64,
    pub min: f64,
    pub condition: String,
    pub icon: String,
    pub precip_prob: Option<u32>,
    pub sunrise: String,
    pub sunset: String,
}

pub struct WeatherService {
    pub current: Arc<RwLock<WeatherData>>,
    pub hourly: Arc<RwLock<Vec<ForecastEntry>>>,
    pub daily: Arc<RwLock<Vec<DailyEntry>>>,
    /// `--lat/--lon`, used while the GPS has no fix.
    fallback: Option<(f64, f64)>,
    gps: Arc<GpsService>,
}

impl WeatherService {
    pub fn new(fallback: Option<(f64, f64)>, gps: Arc<GpsService>) -> Self {
        Self {
            current: Arc::new(RwLock::new(WeatherData::default())),
            hourly: Arc::new(RwLock::new(Vec::new())),
            daily: Arc::new(RwLock::new(Vec::new())),
            fallback,
            gps,
        }
    }

    pub async fn start(self: &Arc<Self>) {
        let first_ok = self.refresh().await;
        let me = self.clone();
        tokio::spawn(async move {
            let mut failures = u32::from(!first_ok);
            loop {
                let period = std::time::Duration::from_secs(300);
                super::wait_for_refresh(failures, period).await;
                if me.refresh().await { failures = 0 } else { failures += 1 }
            }
        });

        match self.fallback {
            Some((lat, lon)) => info!("weather service started (fallback location {lat:.3}, {lon:.3})"),
            None => info!("weather service started (location from GPS only)"),
        }
    }

    /// Latest GPS fix (rounded to ~1 km, enough for a forecast), else the fallback.
    async fn location(&self) -> Option<(f64, f64)> {
        let g = self.gps.current.read().await;
        if g.has_fix {
            return Some(((g.latitude * 100.0).round() / 100.0, (g.longitude * 100.0).round() / 100.0));
        }
        self.fallback
    }

    pub async fn refresh(&self) -> bool {
        let Some((lat, lon)) = self.location().await else { return false };
        refresh_impl(&self.current, &self.hourly, &self.daily, lat, lon).await
    }
}

async fn refresh_impl(
    current: &Arc<RwLock<WeatherData>>,
    hourly: &Arc<RwLock<Vec<ForecastEntry>>>,
    daily: &Arc<RwLock<Vec<DailyEntry>>>,
    lat: f64,
    lon: f64,
) -> bool {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast\
         ?latitude={lat}&longitude={lon}\
         &current=temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,wind_speed_10m\
         &hourly=temperature_2m,weather_code,precipitation_probability\
         &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,sunrise,sunset\
         &forecast_hours=24&forecast_days=7&timezone=auto"
    );

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // Don't follow redirects: reqwest's default policy would happily chase
        // a 302 to an unvetted (possibly internal) host.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => { warn!("weather client build failed: {e}"); return false; }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => { warn!("weather fetch failed: {e}"); return false; }
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => { warn!("weather parse failed: {e}"); return false; }
    };

    let cur = &json["current"];
    let wmo = cur["weather_code"].as_u64().unwrap_or(0) as u32;
    let (condition, icon) = wmo_to_description(wmo);

    *current.write().await = WeatherData {
        temperature: cur["temperature_2m"].as_f64().unwrap_or(0.0),
        feels_like: cur["apparent_temperature"].as_f64().unwrap_or(0.0),
        condition: condition.into(),
        icon: icon.into(),
        humidity: cur["relative_humidity_2m"].as_u64().unwrap_or(0) as u32,
        wind_speed_kmh: cur["wind_speed_10m"].as_f64().unwrap_or(0.0),
    };

    let h = &json["hourly"];
    let times = h["time"].as_array();
    let temps = h["temperature_2m"].as_array();
    let codes = h["weather_code"].as_array();

    if let (Some(times), Some(temps), Some(codes)) = (times, temps, codes) {
        let entries: Vec<ForecastEntry> = times
            .iter()
            .zip(temps.iter())
            .zip(codes.iter())
            .enumerate()
            .map(|(i, ((t, temp), code))| {
                let wmo = code.as_u64().unwrap_or(0) as u32;
                ForecastEntry {
                    time: t.as_str().unwrap_or("").to_string(),
                    temp: temp.as_f64().unwrap_or(0.0),
                    icon: wmo_to_description(wmo).1.into(),
                    precip_prob: h["precipitation_probability"][i].as_u64().map(|v| v as u32),
                }
            })
            .collect();
        *hourly.write().await = entries;
    }

    let d = &json["daily"];
    if let Some(dates) = d["time"].as_array() {
        let days: Vec<DailyEntry> = dates
            .iter()
            .enumerate()
            .map(|(i, date)| {
                let (condition, icon) = wmo_to_description(d["weather_code"][i].as_u64().unwrap_or(0) as u32);
                DailyEntry {
                    date: date.as_str().unwrap_or("").to_string(),
                    max: d["temperature_2m_max"][i].as_f64().unwrap_or(0.0),
                    min: d["temperature_2m_min"][i].as_f64().unwrap_or(0.0),
                    condition: condition.into(),
                    icon: icon.into(),
                    precip_prob: d["precipitation_probability_max"][i].as_u64().map(|v| v as u32),
                    sunrise: d["sunrise"][i].as_str().unwrap_or("").to_string(),
                    sunset: d["sunset"][i].as_str().unwrap_or("").to_string(),
                }
            })
            .collect();
        *daily.write().await = days;
    }
    true
}

fn wmo_to_description(code: u32) -> (&'static str, &'static str) {
    match code {
        0 | 1 => ("Clear", "sun"),
        2 => ("Partly cloudy", "cloud-sun"),
        3 => ("Overcast", "cloud"),
        45 | 48 => ("Foggy", "smog"),
        51..=55 | 61..=65 | 80..=82 => ("Rain", "cloud-rain"),
        71..=75 => ("Snow", "snowflake"),
        95..=99 => ("Thunderstorm", "bolt"),
        _ => ("Unknown", "cloud"),
    }
}
