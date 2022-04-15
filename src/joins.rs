use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use std::time::UNIX_EPOCH;

use crossbeam::sync::ShardedLock;
use serde::{Deserialize, Serialize};
use serenity::model::id;

const BACKLOG_SIZE: usize = 720;

pub struct GuildJoinsMap {
    lock: ShardedLock<HashMap<id::GuildId, Mutex<GuildJoins>>>,
    data_dir: PathBuf,
}

impl GuildJoinsMap {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            lock: ShardedLock::default(),
            data_dir,
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let write = self.lock.write().unwrap();
        for gj in write.values() {
            gj.lock().unwrap().save()?;
        }
        Ok(())
    }

    fn run<F, R>(&self, guild: id::GuildId, f: F) -> R
    where
        F: FnOnce(&mut GuildJoins) -> R,
    {
        {
            let read = self.lock.read().unwrap();
            if let Some(gj) = read.get(&guild) {
                let mut lock = gj.lock().unwrap();
                return f(&mut lock);
            }
        }

        {
            let path = self.data_dir.join(&format!("{}.json", guild));
            let mut write = self.lock.write().unwrap();
            let gj = write
                .entry(guild)
                .or_insert_with(|| Mutex::new(GuildJoins::read_or_new(path)));
            let lock = gj.get_mut().unwrap();
            f(lock)
        }
    }

    pub fn add(&self, guild: id::GuildId, delta: u32) -> io::Result<Stat> {
        self.run(guild, move |gj| {
            gj.add(delta)?;
            gj.stat()
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct GuildJoins {
    current_hour: u64,
    log: VecDeque<Option<u32>>,
    current: u32,
    #[serde(skip)]
    path: PathBuf,
}

impl GuildJoins {
    pub fn read_or_new(path: PathBuf) -> Self {
        Self::read(path.clone()).unwrap_or_else(|_| Self::new(path))
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            current_hour: current_hour(),
            log: std::iter::repeat(None).take(BACKLOG_SIZE).collect(),
            current: 0,
            path,
        }
    }

    pub fn read(path: PathBuf) -> Result<Self, std::io::Error> {
        let f = fs::File::open(&path)?;
        let mut de: Self =
            serde_json::from_reader(f).map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        de.path = path;
        de.update_to_latest_hour(true)?;
        Ok(de)
    }

    pub fn save(&self) -> io::Result<()> {
        let f = fs::File::create(&self.path)?;
        serde_json::to_writer(f, self).map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        Ok(())
    }

    pub fn update_to_latest_hour(&mut self, fill_with_none: bool) -> io::Result<()> {
        let now = current_hour();
        assert!(self.current_hour <= now, "System clock travelled backwards");

        let need_save = self.current_hour != now;

        if self.current_hour < now {
            self.current_hour += 1;
            self.log.pop_front();
            self.log.push_back(Some(self.current));
            self.current = 0;
        }

        // could make this O(1), but no need for that complexity
        while self.current_hour < now {
            self.current_hour += 1;
            self.log.pop_front();
            self.log.push_back(match fill_with_none {
                true => None,
                false => Some(0),
            });
            self.current = 0;
        }

        if need_save {
            self.save()?;
        }

        Ok(())
    }

    pub fn add(&mut self, delta: u32) -> io::Result<()> {
        self.update_to_latest_hour(false)?;
        self.current += delta;
        Ok(())
    }

    pub fn stat(&mut self) -> io::Result<Stat> {
        self.update_to_latest_hour(false)?;

        let mut data: Vec<_> = self
            .log
            .iter()
            .copied()
            .flatten()
            .map(|int| int as f64)
            .collect();
        // we can't have NANs from (int as f64)
        data.sort_by(|a, b| a.partial_cmp(b).unwrap());

        Ok(Stat {
            mean: data.iter().copied().sum::<f64>() / (data.len() as f64),
            max: get_percentile(&data, 1.),
            uq: get_percentile(&data, 0.75),
            median: get_percentile(&data, 0.5),
            lq: get_percentile(&data, 0.25),
            min: get_percentile(&data, 0.),
            n: data.len(),
            current: self.current,
        })
    }
}

#[derive(Debug)]
pub struct Stat {
    mean: f64,
    max: f64,
    uq: f64,
    median: f64,
    lq: f64,
    min: f64,
    n: usize,
    current: u32,
}

impl Stat {
    pub fn is_abnormal(&self) -> bool {
        if self.current <= 8 {
            return false;
        }
        (self.current as f64) > self.uq * 2. + 5.
    }
}

impl fmt::Display for Stat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "Average of {:.3} joins/h in {} samples",
            self.mean, self.n
        )?;
        writeln!(
            f,
            "Quartiles: {:.3} / {:.3} / {:.3} / {:.3} / {:.3}",
            self.min, self.lq, self.median, self.uq, self.max
        )?;
        writeln!(f, "There were {} joins in the past hour.", self.current)?;
        Ok(())
    }
}

fn current_hour() -> u64 {
    UNIX_EPOCH
        .elapsed()
        .expect("System clock is earlire than unix epoch")
        .as_secs()
        / 3600
}

pub fn get_percentile(slice: &[f64], ratio: f64) -> f64 {
    if slice.is_empty() {
        return 0.;
    }
    let position = linterp(0., (slice.len() - 1) as f64, ratio);
    let low = position.trunc() as usize;
    let high = low + 1;
    if high >= slice.len() {
        slice[low]
    } else {
        linterp(slice[low], slice[high], position.fract())
    }
}

pub fn linterp(l: f64, r: f64, k: f64) -> f64 {
    l * (1. - k) + r * k
}
