use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use regex::bytes::{Captures, Regex, RegexSet};
use serde::{de::Error, Deserialize, Deserializer};
use serde_derive::Deserialize;
use std::{
    cmp::Reverse,
    collections::BTreeMap,
    error,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    process, str,
};
use structopt::StructOpt;

struct Interval {
    start: NaiveDateTime,
    end: NaiveDateTime,
    message: Vec<u8>,
    color: usize,
    lane: usize,
}

#[derive(Debug, Deserialize)]
struct TimestampPattern {
    #[serde(deserialize_with = "deserialize_regex")]
    pattern: Regex,
    format: String,
}

#[derive(Debug, Deserialize)]
struct DurationPattern {
    #[serde(deserialize_with = "deserialize_regex")]
    pattern: Regex,
}

#[derive(Debug, Deserialize)]
struct ColorPattern {
    pattern: String,
    color: String,
    #[serde(default)]
    group: usize,
}

#[derive(Debug, Deserialize)]
struct Config {
    timestamp: TimestampPattern,
    durations: Vec<DurationPattern>,
    colors: Vec<ColorPattern>,
}

#[derive(StructOpt)]
struct Args {
    #[structopt(short, long, parse(from_os_str))]
    config: PathBuf,

    #[structopt(parse(from_os_str))]
    log: PathBuf,
}

fn deserialize_regex<'de, D: Deserializer<'de>>(de: D) -> Result<Regex, D::Error> {
    let pattern = <&'de str>::deserialize(de)?;
    Ok(Regex::new(pattern).map_err(D::Error::custom)?)
}

fn get_str<'t>(c: &Captures<'t>, index: usize) -> Option<&'t str> {
    str::from_utf8(c.get(index)?.as_bytes()).ok()
}

fn get_float(c: &Captures, name: &str) -> f64 {
    c.name(name)
        .and_then(|m| str::from_utf8(m.as_bytes()).ok())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn parse_duration(captures: &Captures) -> Option<Duration> {
    let hours = get_float(captures, "h");
    let minutes = get_float(captures, "m");
    let seconds = get_float(captures, "s");
    let nanoseconds = (hours * 3600.0 + minutes * 60.0 + seconds) * 1e9;
    Some(Duration::nanoseconds(nanoseconds.round() as i64))
}

fn run() -> Result<(), Box<dyn error::Error>> {
    let args = Args::from_args();
    let config_bytes = fs::read(args.config)?;
    let config = toml::from_slice::<Config>(&config_bytes)?;

    let color_regex_set = RegexSet::new(config.colors.iter().map(|c| &*c.pattern))?;

    let mut intervals = Vec::new();
    let cutoff = Duration::seconds(1);

    let log_file = BufReader::new(File::open(args.log)?);
    for line in log_file.split(b'\n') {
        let line = line?;
        if let Some(dur_captures) = config
            .durations
            .iter()
            .flat_map(|d| d.pattern.captures(&line))
            .next()
        {
            if let Some(ts_captures) = config.timestamp.pattern.captures(&line) {
                let end_ts = get_str(&ts_captures, 1).unwrap();
                let end_ts = NaiveDateTime::parse_from_str(end_ts, &config.timestamp.format)
                    .or_else(|_| {
                        NaiveTime::parse_from_str(end_ts, &config.timestamp.format)
                            .map(|t| NaiveDate::from_ymd(1, 1, 1).and_time(t))
                    })?;
                if let Some(dur) = parse_duration(&dur_captures) {
                    if dur < cutoff {
                        continue;
                    }
                    let color = color_regex_set
                        .matches(&line)
                        .iter()
                        .next()
                        .ok_or_else(|| {
                            format!("no color specified for {}", String::from_utf8_lossy(&line))
                        })?;
                    let start_ts = end_ts - dur;
                    intervals.push(Interval {
                        start: start_ts,
                        end: end_ts,
                        message: line,
                        color,
                        lane: 0,
                    });
                }
            }
        }
    }

    intervals.sort_unstable_by_key(|a| (a.start, Reverse(a.end)));
    let global_start_time = intervals[0].start;

    let mut lanes = config
        .colors
        .iter()
        .map(|c| (c.group, (0, Vec::new())))
        .collect::<BTreeMap<_, _>>();
    for interval in &mut intervals {
        let group = config.colors[interval.color].group;
        let color_lanes = &mut lanes.get_mut(&group).unwrap().1;
        if let Some((lane_end_time, lane_id)) = color_lanes
            .iter_mut()
            .zip(0..)
            .filter(|(e, _)| **e - interval.start < cutoff)
            .next()
        {
            *lane_end_time = interval.end;
            interval.lane = lane_id;
        } else {
            interval.lane = color_lanes.len();
            color_lanes.push(interval.end);
        }
    }

    let mut total_lanes = 0;
    for (start_lane_id, lanes) in lanes.values_mut() {
        *start_lane_id = total_lanes;
        total_lanes += lanes.len();
    }

    println!(
        "{}",
        r##"<!DOCTYPE html>
            <html>
                <head>
                    <meta charset="utf8">
                    <title>Execution timeline</title>
                    <style>
                        #lanes {
                            position: relative;
                        }
                        .block {
                            position: absolute;
                            width: 0.9em;
                            box-sizing: border-box;
                            border-radius: 0.1em;
                        }
                        .block:hover {
                            border: 1px solid black;
                        }
        "##
    );

    for (i, color_config) in config.colors.iter().enumerate() {
        println!(
            r##"
                .c{} {{
                    background: {};
                }}
            "##,
            i, color_config.color,
        );
    }

    println!(
        "{}",
        r##"        </style>
                </head>
                <body>
                    <p><label for="zoom">Zoom out: </label><input id="zoom" type="range" min="1" max="100" value="1"></p>
                    <div id="lanes">
        "##
    );

    for interval in &intervals {
        let top = (interval.start - global_start_time).num_seconds();
        let height = (interval.end - interval.start).num_seconds();
        let color_config = &config.colors[interval.color];
        println!(
            r##"<div class="block c{}" title="{} ~ {}
{}" style="top:{}px;height:{}px;left:{}em;"></div>
            "##,
            interval.color,
            interval.start,
            interval.end,
            askama_escape::escape(unsafe { str::from_utf8_unchecked(&interval.message) }),
            top,
            height,
            interval.lane + lanes[&color_config.group].0,
        )
    }

    println!(
        "{}",
        r##"
                </div>
                <script>
                    var zoom = document.getElementById('zoom');
                    var lanes = document.getElementById('lanes');
                    zoom.oninput = function(e) {
                        lanes.style.transform = 'scaleY(' + (1/zoom.value) + ')';
                    };
                </script>
            </body>
        </html>
        "##
    );

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}
