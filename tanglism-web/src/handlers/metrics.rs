pub mod atr;
mod ema;
mod ma;

use super::stock_prices::get_stock_tick_prices;
use crate::models::StockTickPrice;
use crate::BasicCfg;
use crate::{DbPool, Error, ErrorKind, Result};
use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime};
use ema::approximate_macd;
use jqdata::JqdataClient;
use serde_derive::*;
use std::collections::HashMap;
use tanglism_utils::{TradingDates, LOCAL_DATES};

#[derive(Debug, Serialize, Deserialize)]
pub struct Response<T> {
    code: String,
    tick: String,
    start_ts: NaiveDateTime,
    end_ts: NaiveDateTime,
    data: T,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Path {
    code: String,
    tick: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Param {
    pub start_dt: String,
    pub end_dt: Option<String>,
    pub metrics_cfg: Option<String>,
}

/// 指标
///
/// 单个时间点的单个指标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub ts: NaiveDateTime,
    pub value: BigDecimal,
}

/// 指标
///
/// 单个时间点的多个指标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiMetric {
    pub ts: NaiveDateTime,
    pub values: Vec<BigDecimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacdMetric {
    pub fast_ema_period: u32,
    pub slow_ema_period: u32,
    pub dea_period: u32,
    pub dif: Vec<Metric>,
    pub dea: Vec<Metric>,
    pub macd: Vec<Metric>,
}

impl Default for MacdMetric {
    fn default() -> Self {
        MacdMetric {
            fast_ema_period: 12,
            slow_ema_period: 26,
            dea_period: 9,
            dif: Vec::new(),
            dea: Vec::new(),
            macd: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MacdCfg {
    fast_ema_period: u32,
    slow_ema_period: u32,
    dea_period: u32,
}

impl Default for MacdCfg {
    fn default() -> Self {
        MacdCfg {
            fast_ema_period: 12,
            slow_ema_period: 26,
            dea_period: 9,
        }
    }
}

pub async fn get_metrics_macd(
    db: &DbPool,
    jq: &JqdataClient,
    basic_cfg: BasicCfg,
    macd_cfg: MacdCfg,
) -> Result<MacdMetric> {
    let fast_ema_period = macd_cfg.fast_ema_period;
    let slow_ema_period = macd_cfg.slow_ema_period;
    let dea_period = macd_cfg.dea_period;
    if slow_ema_period < fast_ema_period || slow_ema_period < dea_period {
        return Err(Error::custom(
            ErrorKind::BadRequest,
            format!(
                "invalid setting: slow ema {} is no less than fast ema {} or dea {}",
                slow_ema_period, fast_ema_period, dea_period
            ),
        ));
    }
    let search_start_dt =
        ema_approximate_start(basic_cfg.start_ts.date(), &basic_cfg.tick, slow_ema_period)?;
    let prices = get_stock_tick_prices(
        &db,
        &jq,
        &basic_cfg.tick,
        &basic_cfg.code,
        search_start_dt.and_hms(0, 0, 0),
        basic_cfg.end_ts,
    )
    .await?;
    let (dif_raw, dea_raw, macd_raw) = approximate_macd(
        &prices,
        fast_ema_period,
        slow_ema_period,
        dea_period,
        |p| p.close.clone(),
        |p| p.ts,
    );
    let dif = dif_raw
        .into_iter()
        .filter(|d| d.ts >= basic_cfg.start_ts)
        .collect();
    let dea = dea_raw
        .into_iter()
        .filter(|d| d.ts >= basic_cfg.start_ts)
        .collect();
    let macd = macd_raw
        .into_iter()
        .filter(|d| d.ts >= basic_cfg.start_ts)
        .collect();
    Ok(MacdMetric {
        fast_ema_period,
        slow_ema_period,
        dea_period,
        dif,
        dea,
        macd,
    })
}

pub fn parse_macd_cfg(s: &str) -> Option<MacdCfg> {
    let mut fast_ema_period = None;
    let mut slow_ema_period = None;
    let mut dea_period = None;
    for c in s.split(',') {
        if c.starts_with("fast_ema:") {
            if let Ok(n) = c[9..].parse() {
                fast_ema_period = Some(n);
            }
        } else if c.starts_with("slow_ema:") {
            if let Ok(n) = c[9..].parse() {
                slow_ema_period = Some(n);
            }
        } else if c.starts_with("dea:") {
            if let Ok(n) = c[4..].parse() {
                dea_period = Some(n);
            }
        }
    }
    match (fast_ema_period, slow_ema_period, dea_period) {
        (Some(fast_ema_period), Some(slow_ema_period), Some(dea_period)) => Some(MacdCfg {
            fast_ema_period,
            slow_ema_period,
            dea_period,
        }),
        _ => None,
    }
}

fn ema_approximate_start(start_dt: NaiveDate, tick: &str, period: u32) -> Result<NaiveDate> {
    // 计算额外所需的价格序列的起始区间
    // 3.5 * 周期，之前的价格影响很小
    let total_period = (3.50_f64 * period as f64) as i64;
    let day_factor = match tick {
        "1m" => 240,
        "5m" => 48,
        "30m" => 8,
        "1d" => 1,
        _ => {
            return Err(Error::custom(
                ErrorKind::BadRequest,
                format!("invalid tick: {}", tick),
            ))
        }
    };
    let offset_days = (total_period / day_factor + 1) as i64;

    let mut dt = start_dt;
    for _i in 0..offset_days {
        if let Some(prev_dt) = LOCAL_DATES.prev_day(dt) {
            dt = prev_dt;
        } else {
            // 超过边界
            return Err(Error::custom(
                ErrorKind::InternalServerError,
                "exceeds time limit".to_owned(),
            ));
        }
    }
    Ok(dt)
}

/// 简化多股票ATRP统计
///
/// 输入必定是按照code和ts升序排列的
pub fn multi_atrp_stats(input: &[StockTickPrice]) -> HashMap<String, atr::AtrpStats> {
    let mock_tail = vec![mock_price_placeholder()];
    let atrp_agg = input
        .iter()
        .skip(1)
        .chain(mock_tail.iter())
        .zip(input.iter())
        .fold(AtrpContainer::empty(), |mut acc, (curr, prev)| {
            if curr.code == prev.code {
                acc.add_item(atr::AtrInput {
                    ts: curr.ts,
                    curr_high: curr.high.clone(),
                    curr_low: curr.low.clone(),
                    prev_close: prev.close.clone(),
                });
            } else {
                acc.flush(prev.code.to_owned());
            }
            acc
        });
    atrp_agg.rst
}

fn mock_price_placeholder() -> StockTickPrice {
    StockTickPrice {
        tick: "1d".to_owned(),
        code: "NONEXISTS".to_owned(),
        ts: NaiveDate::from_ymd(1970, 1, 1).and_hms(0, 0, 0),
        open: BigDecimal::from(0),
        close: BigDecimal::from(0),
        high: BigDecimal::from(0),
        low: BigDecimal::from(0),
        volume: BigDecimal::from(0),
        amount: BigDecimal::from(0),
    }
}

struct AtrpContainer {
    rst: HashMap<String, atr::AtrpStats>,
    tmp: Vec<atr::AtrInput>,
}

impl AtrpContainer {
    fn empty() -> Self {
        AtrpContainer {
            rst: HashMap::new(),
            tmp: Vec::new(),
        }
    }

    fn add_item(&mut self, item: atr::AtrInput) {
        self.tmp.push(item);
    }

    fn flush(&mut self, code: String) {
        if self.tmp.is_empty() {
            return;
        }
        let stats = atr::atrp_stats(self.tmp.drain(..));
        self.rst.insert(code, stats);
    }
}
