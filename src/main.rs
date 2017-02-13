extern crate daemonize;
extern crate simplelog;
extern crate getopts;
extern crate toml;
extern crate glob;
extern crate libc;
#[macro_use]
extern crate log;

use std::io::prelude::*;
use std::fs::{File, OpenOptions};
use daemonize::Daemonize;
use simplelog::{WriteLogger, LogLevelFilter, Config as LoggerConfig, TermLogger};
use kalman::Kalman;
use config::Config;
use getopts::Options;
use std::env;

mod kalman;
mod config;
mod discrete_value;
mod switch_monitor;

#[derive(Debug)]
struct LightConvertor {
    points: Vec<LightPoint>,
}

impl LightConvertor {
    fn new(mut points: Vec<LightPoint>) -> Self {
        points.sort_by(|p1, p2| p1.illuminance.cmp(&p2.illuminance));
        if points[0].illuminance != 0 {
            points.insert(0,
                          LightPoint {
                              illuminance: 0,
                              light: 0,
                          });
        }
        debug!("Points: {:?}", points);
        LightConvertor { points: points }
    }

    fn get_light(&self, illuminance: u32) -> f32 {
        match self.points
            .iter()
            .position(|p| illuminance < p.illuminance) {
            None => self.points.last().unwrap().light as f32,
            Some(0) => self.points[0].light as f32,
            Some(right_index) => {
                debug!("right index: {}", right_index);
                let left = &self.points[right_index - 1];
                let right = &self.points[right_index];
                let diff = (right.illuminance - left.illuminance) as f32;
                if diff == 0.0 {
                    left.light as f32
                } else {
                    (right.light - left.light) as f32 / diff *
                    (illuminance - left.illuminance) as f32 + left.light as f32
                }
            }
        }
    }
}

fn read_file_to_string(filename: &str) -> std::io::Result<String> {
    let mut fd = File::open(filename)?;
    let mut s = String::new();
    fd.read_to_string(&mut s)?;
    Ok(s)
}

fn read_file_to_u32(filename: &str) -> Option<u32> {
    read_file_to_string(filename)
        .map_err(|e| {
            error!("Cannot read file `{}`: {}", filename, e);
            e
        })
        .ok()
        .and_then(|s| {
            s.trim_right()
                .parse::<u32>()
                .map_err(|e| format!("Cannot parse {} as integer: {} from `{}`", s, e, filename))
                .ok()
        })
}

fn write_u32_to_file(filename: &str, value: u32) -> std::io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(filename)
        .and_then(|mut fd| fd.write_all(value.to_string().as_ref()))
        .map_err(|e| {
            error!("Cannot write to file `{}` error: {}", filename, e);
            e
        })
}

fn main_loop(config: &Config,
             light_convertor: &LightConvertor,
             max_brightness: u32,
             mut switch_monitor: switch_monitor::SwitchMonitor)
             -> Result<(), ErrorCode> {
    let mut kalman = Kalman::new(config.kalman_q(),
                                 config.kalman_r(),
                                 config.kalman_covariance());
    let mut stepped_brightness = discrete_value::DiscreteValue::new(config.min_backlight(),
                                                                    max_brightness,
                                                                    config.light_steps(),
                                                                    config.step_barrier());
    debug!("k: s:{:?}", stepped_brightness);
    loop {
        match read_file_to_u32(config.illuminance_filename()) {
            Some(illuminance) => {
                let illuminance_k = kalman.process(illuminance as f32);
                let brightness = light_convertor.get_light(illuminance_k as u32);
                debug!("{}, {}, {}", illuminance, illuminance_k, brightness);
                if let Some(new) = stepped_brightness.update(brightness) {
                    info!("row {}, kalman {}, new level {} new brightness {}",
                          illuminance,
                          illuminance_k,
                          brightness,
                          new);
                    if let Err(e) = write_u32_to_file(config.backlight_filename(), new) {
                        error!("Cannot set brightness: {}", e);
                    }
                }
            }
            _ => error!("Cannot read illuminance"),
        }
        if switch_monitor.try_receive_event(config.check_period_in_seconds()) {
            info!("disabled by event, wait for enabling");
            while !switch_monitor.try_receive_event(60 * 60) {
            }
            info!("enabled by event");
        }
    }
}

fn print_usage(program: &str, opts: Options) {
    let brief = format!("Usage: {} [options]", program);
    print!("{}", opts.usage(&brief));
}

#[derive(Debug)]
pub struct LightPoint {
    illuminance: u32,
    light: u32,
}

pub enum ErrorCode {
    InvalidArgs,
    ConfigReadError,
    ConfigParseError,
    TracerCreateError,
    DaemonizeErrror,
    ReadMaxBrightnessError,
    InvalidPointsInConfig,
    ReadBacklightError,
    ReadIlluminanceError,
    CannotSetBacklight,
    SyslogOpenError,
}

fn parse_config(config: &String) -> Result<toml::Table, ErrorCode> {
    let mut config_parser = toml::Parser::new(&config);
    config_parser.parse()
        .ok_or_else(|| {
            println!("Cannot parse config file:");
            for ref e in config_parser.errors.iter() {
                println!("{} at `{}`", e, &config[e.lo..e.hi]);
            }
            ErrorCode::ConfigParseError
        })
}

fn run() -> Result<(), ErrorCode> {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();

    let mut opts = Options::new();
    opts.optopt("o", "log", "set log file", "filename");
    opts.optopt("p", "pid", "set pid file", "filename");
    opts.optopt("c", "config", "config file", "filename");
    opts.optflag("v", "version", "print version");
    opts.optflag("d", "no-fork", "no fork for debug");
    opts.optflag("h", "help", "print this help");
    opts.optflag("k", "stop", "send SIGTERM signal");
    // opts.optflag("r", "reload", "send SIGHUP signal");

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            println!("{}", &f);
            print_usage(&program, opts);
            return Err(ErrorCode::InvalidArgs);
        }
    };

    if matches.opt_present("help") || !matches.free.is_empty() {
        print_usage(&program, opts);
        return Ok(());
    };

    let config = if let Some(config_filename) = matches.opt_str("config") {
        let f = read_file_to_string(&config_filename);
        if let Err(e) = f {
            println!("Cannot open config file `{}`: {}", config_filename, e);
            return Err(ErrorCode::ConfigReadError);
        }
        Config::new(parse_config(&f.unwrap()).ok())
    } else {
        let default = "/usr/local/etc/illuminanced.toml";
        let f = read_file_to_string(&default);
        if let Err(ref e) = f {
            println!("Cannot open config file `{}`: {}, ignore", default, e);
        }
        Config::new(f.ok().and_then(|f| parse_config(&f).ok()))
    };

    if matches.opt_present("stop") {
        return Daemonize::new()
            .pid_file(config.pid_filename())
            .stop()
            .map_err(|e| {
                println!("Cannot stop: {}", e);
                ErrorCode::DaemonizeErrror
            });
    }

    if matches.opt_present("no-fork") {
        let _ = TermLogger::init(LogLevelFilter::Debug, LoggerConfig::default());
    } else {
        if matches.opt_present("log") {
            let log_filename = matches.opt_str("log").unwrap_or(config.log_filename().to_string());
            let log_file = OpenOptions::new().append(true)
                .create(true)
                .open(&log_filename)
                .map_err(|e| {
                    println!("Cannot open log file: `{}`, error: {}", log_filename, e);
                    ErrorCode::TracerCreateError
                })?;
            WriteLogger::init(config.log_level(), LoggerConfig::default(), log_file).map_err(|e| {
                    println!("Cannot create logger: {}", e);
                    ErrorCode::TracerCreateError
                })?;
        }
    }

    let light_points = config.light_points()?;
    let light_convertor = LightConvertor::new(light_points);
    let max_brightness =
        read_file_to_u32(config.max_backlight_filename()).ok_or(ErrorCode::ReadMaxBrightnessError)?;
    read_file_to_u32(config.illuminance_filename()).ok_or_else(|| {
            error!("Cannot read from {}", config.illuminance_filename());
            ErrorCode::ReadIlluminanceError
        })?;
    let brightness = read_file_to_u32(config.backlight_filename()).ok_or_else(|| {
            error!("Cannot read from {}", config.backlight_filename());
            ErrorCode::ReadBacklightError
        })?;
    write_u32_to_file(config.backlight_filename(), brightness)
        .map_err(|_| ErrorCode::CannotSetBacklight)?;

    let switch_monitor = switch_monitor::SwitchMonitor::new(config.event_device_mask(),
                                                            config.event_device_name());

    if !matches.opt_present("no-fork") {
        Daemonize::new().pid_file(config.pid_filename())
            .start()
            .map_err(|e| {
                error!("Cannot daemonize: {}", e);
                ErrorCode::DaemonizeErrror
            })?;
    }

    main_loop(&config, &light_convertor, max_brightness, switch_monitor)
}

fn main() {
    std::process::exit(match run() {
        Ok(()) => 0i32,
        Err(x) => x as i32,
    })
}
