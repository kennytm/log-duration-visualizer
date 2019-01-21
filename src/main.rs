use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use regex::bytes::{Captures, Regex, RegexSet};
use serde::{de::Error, Deserialize, Deserializer};
use serde_derive::Deserialize;
use std::{
    cmp::Reverse,
    collections::BTreeMap,
    error,
    fs::{self, File},
    io::{BufRead, BufReader, stdout, Write},
    path::PathBuf,
    process, str,
    borrow::Cow,
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

fn escape_js(s: &str) -> Cow<'_, str> {
    lazy_static::lazy_static! {
        static ref PATTERN: regex::Regex = regex::Regex::new("['\\\\\r\n]").unwrap();
    }
    PATTERN.replace_all(s, |c: &regex::Captures| {
        match c.get(0).unwrap().as_str() {
            r"'" => r"\'",
            r"\" => r"\\",
            "\r" => r"\r",
            "\n" => r"\n",
            _ => unreachable!(),
        }
    })
}

const LANE_WIDTH: usize = 20;
const MIN_GLOBAL_WIDTH: usize = 400;

fn run() -> Result<(), Box<dyn error::Error>> {
    let args = Args::from_args();
    let config_bytes = fs::read(args.config)?;
    let config = toml::from_slice::<Config>(&config_bytes)?;

    let color_regex_set = RegexSet::new(config.colors.iter().map(|c| &*c.pattern))?;

    let mut intervals = Vec::new();
    let cutoff = Duration::seconds(1);

    let log_file = BufReader::new(File::open(args.log)?);

    let mut global_start_time = chrono::naive::MAX_DATE.and_hms_nano(23, 59, 59, 999_999_999);
    let mut global_end_time = chrono::naive::MIN_DATE.and_hms(0, 0, 0);

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
                    if start_ts < global_start_time {
                        global_start_time = start_ts;
                    }
                    if end_ts > global_end_time {
                        global_end_time = end_ts;
                    }
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

    let global_start_time = global_start_time;
    let global_end_time = global_end_time;
    let global_duration = (global_end_time - global_start_time).num_seconds();

    intervals.sort_unstable_by_key(|a| (a.start, Reverse(a.end)));

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

    let stdout = stdout();
    let mut lock = stdout.lock();

    writeln!(
        lock,
        r##"<!DOCTYPE html>
            <html>
                <head>
                    <meta charset="utf8">
                    <title>Execution timeline</title>
                    <style>
                        canvas {{
                            position: absolute;
                            left: 0.5em;
                            top: 0.5em;
                        }}
                        #aux {{
                            position: fixed;
                            right: 0.5em;
                            top: 0.5em;
                            width: 30em;
                            font-family: sans-serif;
                            font-size: 0.75em;
                        }}
                    </style>
                </head>
                <body>
                    <canvas id="lanes" width="{0}" height="{1}"></canvas>
                    <canvas id="hover" width="{0}" height="{1}"></canvas>
                    <div id="aux">
                        <p>
                            <label for="zoom"><strong>Zoom out:</strong></label>
                            <input id="zoom" type="range" min="1" max="100" value="1">
                            (<output for="zoom" id="zoom-val">1</output>×)
                        </p>
                        <p><strong>Start time:</strong> <span id="start-time"></span></p>
                        <p><strong>End time:</strong> <span id="end-time"></span></p>
                        <p><strong>Message:</strong><br/><span id="msg"></span></p>
                    </div>
                    <script>
                        var zoom = document.getElementById('zoom');
                        var globalWidth = {0};
                        var globalHeight = {1};
                        var laneWidth = {2};
                        var colors = [
        "##,
        MIN_GLOBAL_WIDTH.max(total_lanes * LANE_WIDTH),
        global_duration,
        LANE_WIDTH,
    )?;

    for color_config in &config.colors {
        writeln!(lock, "'{}',", escape_js(&color_config.color))?;
    }

    writeln!(
        lock,
        r##"
                        ];
                        var blocks = [
        "##,
    )?;

    for interval in &intervals {
        let top = (interval.start - global_start_time).num_seconds();
        let height = (interval.end - interval.start).num_seconds();
        let color_config = &config.colors[interval.color];
        writeln!(
            lock,
            "{{color: {}, start: '{}', end: '{}', msg: '{}', top: {}, height: {}, lane: {}}},",
            interval.color,
            interval.start,
            interval.end,
            escape_js(&String::from_utf8_lossy(&interval.message)),
            top,
            height,
            interval.lane + lanes[&color_config.group].0,
        )?;
    }

    writeln!(
        lock,
        "{}",
        r##"
                        ];
                        function render(z) {
                            var ctx = document.getElementById('lanes').getContext('2d');
                            ctx.clearRect(0, 0, globalWidth, globalHeight);

                            ctx.lineWidth = 1;
                            ctx.font = 'sans-serif';
                            ctx.textBaseline = 'top';
                            ctx.textAlign = 'right';
                            ctx.fillStyle = '#999';
                            for (var i = 0; i < globalHeight; i += 300) {
                                var notHour = i % 3600;
                                var x = notHour ? 0.85 : 0.75;
                                var y = Math.round(i * z) + 0.5;
                                ctx.strokeStyle = notHour ? '#999' : '#333';
                                ctx.beginPath();
                                ctx.moveTo(globalWidth*x, y);
                                ctx.lineTo(globalWidth, y);
                                ctx.stroke();
                                ctx.fillText((i/60|0) + 'm', globalWidth, y);
                            }

                            for (var i = 0, block; block = blocks[i]; ++ i) {
                                ctx.fillStyle = colors[block.color];
                                ctx.fillRect(
                                    block.lane * laneWidth,
                                    block.top * z,
                                    laneWidth - 1,
                                    block.height * z,
                                );
                            }
                        }
                        document.addEventListener('DOMContentLoaded', function() {
                            render(1);
                        });
                        zoom.addEventListener('input', function() {
                            document.getElementById('zoom-val').value = zoom.value;
                            render(1/zoom.value);
                        });

                        document.getElementById('hover').addEventListener('mousemove', function(e) {
                            var rect = this.getBoundingClientRect();
                            var z = 1/zoom.value;
                            var xx = e.clientX - rect.left;
                            var yy = e.clientY - rect.top;
                            var x = xx / laneWidth;
                            var y = yy / z;
                            yy = Math.round(yy) + 0.5;

                            // FIXME: Consider switching to a spatial data structure to speed up searching
                            // Ref: https://stackoverflow.com/questions/7727758/find-overlapping-rectangles-algorithm
                            var i = 0, block;
                            for (; block = blocks[i]; ++ i) {
                                if (
                                    block.top <= y && y <= block.top + block.height &&
                                    block.lane <= x && x <= block.lane + 1
                                ) {
                                    break;
                                }
                            }
                            if (i >= blocks.length) {
                                i = -1;
                            }

                            var ctx = this.getContext('2d');
                            ctx.clearRect(0, 0, globalWidth, globalHeight);

                            ctx.strokeStyle = 'rgba(255,0,0,0.5)';
                            ctx.lineWidth = 1;
                            ctx.font = 'sans-serif';
                            ctx.textBaseline = 'top';
                            ctx.textAlign = 'left';
                            ctx.fillStyle = '#f88';
                            ctx.beginPath();
                            ctx.moveTo(0, yy);
                            ctx.lineTo(globalWidth, yy);
                            ctx.stroke();
                            ctx.fillText((y/60|0) + 'm' + (y%60|0) + 's', globalWidth * 0.85, yy);

                            if (i !== -1) {
                                var block = blocks[i];
                                ctx.strokeStyle = '#000';
                                ctx.strokeRect(
                                    block.lane * laneWidth,
                                    block.top * z,
                                    laneWidth - 1,
                                    block.height * z,
                                );
                                document.getElementById('start-time').innerText = block.start;
                                document.getElementById('end-time').innerText = block.end;
                                document.getElementById('msg').innerText = block.msg;
                            }
                        });
                    </script>
                </body>
            </html>
        "##,
    )?;

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}
