#![allow(unused)]
use std::io::SeekFrom;
use std::{collections::HashMap, path::Path, time::Duration};

use async_trait::async_trait;
use clap::Parser;
use futures::stream::StreamExt;
use rand::Rng;
use rand::{seq::SliceRandom, RngCore};
use rand_chacha::{rand_core::SeedableRng, ChaCha8Rng};
use rdkafka::{
    consumer::{stream_consumer::StreamConsumer, Consumer},
    ClientConfig, Message, TopicPartitionList,
};
use redis::AsyncCommands;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tracing::{error, info, trace, warn};
use tracing_subscriber;

enum Errors {
    KafkaConnectionError,
    NoKafkaMessage,
    RedisConnectionError,
    RedisKeyRetrievalError,
    FileOpenError,
    FileReadError,
    FileWriteError,
    FileSyncError,
}

impl std::fmt::Debug for Errors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Errors::KafkaConnectionError => write!(f, "Kafka connection error"),
            Errors::NoKafkaMessage => write!(f, "No Kafka message"),
            Errors::RedisConnectionError => write!(f, "Redis connection error"),
            Errors::RedisKeyRetrievalError => write!(f, "Error retrieving redis key"),
            Errors::FileOpenError => write!(f, "Failed to open file"),
            Errors::FileReadError => write!(f, "Failed to read from file"),
            Errors::FileWriteError => write!(f, "Failed to write to file"),
            Errors::FileSyncError => write!(f, "Failed to sync file"),
        }
    }
}

impl std::fmt::Display for Errors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Errors::KafkaConnectionError => write!(f, "Kafka connection error"),
            Errors::NoKafkaMessage => write!(f, "No Kafka message"),
            Errors::RedisConnectionError => write!(f, "Redis connection error"),
            Errors::RedisKeyRetrievalError => write!(f, "Error retrieving redis key"),
            Errors::FileOpenError => write!(f, "Failed to open file"),
            Errors::FileReadError => write!(f, "Failed to read from file"),
            Errors::FileWriteError => write!(f, "Failed to write to file"),
            Errors::FileSyncError => write!(f, "Failed to sync file"),
        }
    }
}

impl std::error::Error for Errors {}

#[derive(Eq, PartialEq, Hash)]
enum FaultType {
    KafkaConnectionFailure,
    KafkaReadFailure,
    RedisConnectionFailure,
    RedisReadFailure,
    FileOpenFailure,
    FileWriteFailure,
}

#[derive(Eq, PartialEq, Hash)]
enum FileFaultType {
    FileReadFailure,
    FileWriteFailure,
    FileSizeExceededFailure,
    FileMetadataSyncFailure,
}

#[derive(Parser, Debug)]
#[command(name = "SimulatIOn", version = "1.0", author = "Zaid Humayun")]
struct Args {
    #[arg(short, long)]
    simulate: bool,
}

#[async_trait]
trait Clock {
    async fn sleep(&mut self, duration: Duration);
}

struct RealClock;

impl RealClock {
    fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Clock for RealClock {
    async fn sleep(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

struct SimulatedClock {
    current_time: Duration,
}

impl SimulatedClock {
    fn new() -> Self {
        Self {
            current_time: Duration::ZERO,
        }
    }

    fn advance(&mut self, duration: Duration) {
        self.current_time += duration;
    }
}

#[async_trait]
impl Clock for SimulatedClock {
    async fn sleep(&mut self, duration: Duration) {
        self.advance(duration);
    }
}

#[async_trait]
trait File {
    async fn read(&mut self, size: usize) -> Result<Vec<u8>, Errors>;
    async fn write(&mut self, data: &str) -> Result<usize, Errors>;
    async fn fsync(&mut self) -> Result<(), Errors>;
    async fn read_last_n_entries(&mut self, n: usize) -> Result<Vec<String>, Errors>;
}

struct RealFile {
    file: Option<tokio::fs::File>,
}

#[async_trait]
impl File for RealFile {
    async fn read(&mut self, size: usize) -> Result<Vec<u8>, Errors> {
        let mut buffer = vec![0; size];
        self.file
            .as_mut()
            .unwrap()
            .read(&mut buffer)
            .await
            .map_err(|_| Errors::FileReadError)?;
        Ok(buffer)
    }

    async fn write(&mut self, data: &str) -> Result<usize, Errors> {
        self.file
            .as_mut()
            .unwrap()
            .write(data.as_bytes())
            .await
            .map_err(|_| Errors::FileWriteError)
    }

    async fn fsync(&mut self) -> Result<(), Errors> {
        self.file
            .as_mut()
            .unwrap()
            .sync_all()
            .await
            .map_err(|_| Errors::FileSyncError)
    }

    async fn read_last_n_entries(&mut self, n: usize) -> Result<Vec<String>, Errors> {
        let file = self.file.as_mut().ok_or(Errors::FileReadError)?;

        // Get file size and seek to end
        let file_size = file
            .metadata()
            .await
            .map_err(|_| Errors::FileReadError)?
            .len() as usize;
        file.seek(SeekFrom::End(0))
            .await
            .map_err(|_| Errors::FileReadError)?;

        // Read chunks from end until we find n newlines
        let mut buffer = Vec::new();
        let mut position = file_size;
        let chunk_size = 1024; // Read 1KB at a time

        while position > 0 && buffer.iter().filter(|&&c| c == b'\n').count() <= n {
            let read_size = std::cmp::min(position, chunk_size);
            position = position.saturating_sub(read_size);

            file.seek(SeekFrom::Start(position as u64))
                .await
                .map_err(|_| Errors::FileReadError)?;

            let mut chunk = vec![0; read_size];
            file.read_exact(&mut chunk)
                .await
                .map_err(|_| Errors::FileReadError)?;

            buffer.splice(0..0, chunk);
        }

        // Convert to string and get last n lines
        let result = String::from_utf8_lossy(&buffer)
            .lines()
            .rev()
            .take(n)
            .map(String::from)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        Ok(result)
    }
}

struct SimulatedFile {
    rng: ChaCha8Rng,
    file_contents: Vec<u8>,
    synced_contents: Vec<u8>,
    current_file_size: usize,
    max_file_size: usize,
    inner: RealFile,
    read_position: usize,
    write_position: usize,
    fault_probabilities: HashMap<FileFaultType, f64>,
}

impl SimulatedFile {
    fn new(rng: ChaCha8Rng, io: RealFile) -> Self {
        let fault_probabilities = HashMap::from([
            (FileFaultType::FileReadFailure, 0.1),
            (FileFaultType::FileWriteFailure, 0.1),
            (FileFaultType::FileSizeExceededFailure, 0.1),
            (FileFaultType::FileMetadataSyncFailure, 0.1),
        ]);
        Self {
            rng,
            file_contents: Vec::new(),
            synced_contents: Vec::new(),
            current_file_size: 0,
            max_file_size: 0,
            inner: io,
            read_position: 0,
            write_position: 0,
            fault_probabilities,
        }
    }

    fn should_inject_fault(&mut self, fault_type: &FileFaultType) -> bool {
        if let Some(&probability) = self.fault_probabilities.get(fault_type) {
            self.rng.gen_bool(probability)
        } else {
            false
        }
    }
}

#[async_trait]
impl File for SimulatedFile {
    async fn read(&mut self, size: usize) -> Result<Vec<u8>, Errors> {
        if self.should_inject_fault(&FileFaultType::FileReadFailure) {
            warn!("Injecting fault while reading from file");
            return Err(Errors::FileReadError);
        }
        assert!(size < self.file_contents.len());
        let buffer = self.file_contents[self.read_position..self.read_position + size].to_vec();
        self.read_position += size;
        Ok(buffer)
    }

    async fn write(&mut self, data: &str) -> Result<usize, Errors> {
        if self.should_inject_fault(&FileFaultType::FileWriteFailure) {
            warn!("Injecting fault while writing to file");
            return Err(Errors::FileWriteError);
        }
        let data = data.as_bytes();
        let write_size = data.len();
        if self.current_file_size + write_size > self.max_file_size {
            return Err(Errors::FileWriteError);
        }
        self.file_contents[self.write_position..self.write_position + write_size]
            .copy_from_slice(&data[..write_size]);
        self.write_position += write_size;
        self.current_file_size += write_size;
        Ok(write_size)
    }

    async fn fsync(&mut self) -> Result<(), Errors> {
        //  TODO: Should we inject failure for fsync? Seems excessive. How do people program around that?
        self.synced_contents = self.file_contents.clone();
        Ok(())
    }

    async fn read_last_n_entries(&mut self, n: usize) -> Result<Vec<String>, Errors> {
        // Since we're writing newline-delimited entries, split on newlines
        let contents = String::from_utf8_lossy(&self.file_contents);
        let entries: Vec<String> = contents
            .lines()
            .rev() // reverse to get last entries
            .take(n) // take last n
            .map(String::from)
            .collect();
        Ok(entries)
    }
}

#[async_trait]
trait IO {
    async fn create_kafka_consumer(
        &mut self,
        group_id: &str,
        broker: &str,
        topic: &str,
        partition: i32,
    ) -> Result<(), Errors>;
    async fn connect_to_redis(&mut self, url: &str) -> Result<(), Errors>;
    async fn open_file(&mut self, path: &Path) -> Result<(), Errors>;
    async fn read_kafka_message(&mut self) -> Result<Option<String>, Errors>;
    async fn get_redis_config(&mut self, key: &str) -> Result<String, Errors>;
    async fn read_file(&mut self, size: usize) -> Result<Vec<u8>, Errors>;
    async fn read_last_n_entries(&mut self, n: usize) -> Result<Vec<String>, Errors>;
    async fn write_to_file(&mut self, data: &str) -> Result<usize, Errors>;
    fn generate_jitter(&mut self, base_delay: Duration) -> Duration;
    async fn sleep(&mut self, duration: Duration);
}

struct RealIO {
    consumer: Option<StreamConsumer>,
    redis_connection: Option<redis::aio::MultiplexedConnection>,
    file: Option<RealFile>,
    pub clock: Box<dyn Clock + Send>,
}

impl RealIO {
    fn new() -> Self {
        let clock = Box::new(RealClock::new());
        Self {
            consumer: None,
            redis_connection: None,
            file: None,
            clock,
        }
    }
}

#[async_trait]
impl IO for RealIO {
    async fn create_kafka_consumer(
        &mut self,
        group_id: &str,
        broker: &str,
        topic: &str,
        partition: i32,
    ) -> Result<(), Errors> {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("group.id", group_id)
            .set("bootstrap.servers", broker)
            .create()
            .map_err(|_| Errors::KafkaConnectionError)?;
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition_offset(topic, partition, rdkafka::Offset::Beginning)
            .map_err(|_| Errors::KafkaConnectionError)?;
        consumer
            .assign(&tpl)
            .map_err(|_| Errors::KafkaConnectionError)?;

        self.consumer = Some(consumer);
        Ok(())
    }

    async fn connect_to_redis(&mut self, url: &str) -> Result<(), Errors> {
        let client = redis::Client::open(url).map_err(|_| Errors::RedisConnectionError)?;
        let connection = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|_| Errors::RedisConnectionError)?;
        self.redis_connection = Some(connection);
        Ok(())
    }

    async fn open_file(&mut self, path: &Path) -> Result<(), Errors> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(path)
            .await
            .map_err(|_| Errors::FileOpenError)?;
        self.file = Some(RealFile { file: Some(file) });
        Ok(())
    }

    async fn read_kafka_message(&mut self) -> Result<Option<String>, Errors> {
        if let Some(consumer) = &self.consumer {
            let message = consumer.stream().next().await;
            let msg = match message {
                Some(Ok(msg)) => msg
                    .payload()
                    .map(|payload| String::from_utf8_lossy(payload).into_owned()),
                _ => return Err(Errors::NoKafkaMessage),
            };
            return Ok(msg);
        }
        Ok(None)
    }

    async fn get_redis_config(&mut self, key: &str) -> Result<String, Errors> {
        if let Some(redis_conn) = &mut self.redis_connection {
            match redis_conn.get(key).await {
                Ok(value) => Ok(value),
                Err(_) => Err(Errors::RedisKeyRetrievalError),
            }
        } else {
            Err(Errors::RedisConnectionError)
        }
    }

    async fn read_file(&mut self, size: usize) -> Result<Vec<u8>, Errors> {
        self.file.as_mut().unwrap().read(size).await
    }

    async fn write_to_file(&mut self, data: &str) -> Result<usize, Errors> {
        self.file.as_mut().unwrap().write(data).await
    }

    async fn read_last_n_entries(&mut self, n: usize) -> Result<Vec<String>, Errors> {
        self.file.as_mut().unwrap().read_last_n_entries(n).await
    }

    fn generate_jitter(&mut self, base_delay: Duration) -> Duration {
        let jitter: u64 = rand::thread_rng().gen_range(0..base_delay.as_millis() as u64);
        base_delay + Duration::from_millis(jitter)
    }

    async fn sleep(&mut self, duration: Duration) {
        self.clock.sleep(duration).await;
    }
}

struct SimulatedIO {
    rng: ChaCha8Rng,
    fault_probabilities: HashMap<FaultType, f64>,
    kafka_messages: Vec<String>,
    kafka_attempts: usize,
    kafka_failures: usize,
    redis_data: HashMap<String, String>,
    file: Option<SimulatedFile>,
    clock: Box<dyn Clock + Send>,
}

impl SimulatedIO {
    fn new(seed: u64) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let clock = Box::new(SimulatedClock::new());
        let kafka_messages = vec![
            "simulated_message_1".to_string(),
            "simulated_message_2".to_string(),
            "simulated_message_3".to_string(),
        ];
        let mut redis_data = HashMap::new();
        redis_data.insert(
            "config_key".to_string(),
            "simulated_config_value".to_string(),
        );
        let fault_probabilities = HashMap::from([
            (FaultType::KafkaConnectionFailure, 0.1),
            (FaultType::KafkaReadFailure, 0.1),
            (FaultType::RedisConnectionFailure, 0.1),
            (FaultType::RedisReadFailure, 0.1),
            (FaultType::FileOpenFailure, 0.1),
            (FaultType::FileWriteFailure, 0.1),
        ]);
        let kafka_failures = rng.gen_range(1..5);

        Self {
            rng,
            fault_probabilities,
            kafka_messages,
            redis_data,
            file: None,
            kafka_attempts: 0,
            kafka_failures,
            clock,
        }
    }

    fn should_inject_fault(&mut self, fault_type: &FaultType) -> bool {
        if let Some(&probability) = self.fault_probabilities.get(fault_type) {
            self.rng.gen_bool(probability)
        } else {
            false
        }
    }
}

#[async_trait]
impl IO for SimulatedIO {
    async fn create_kafka_consumer(
        &mut self,
        _group_id: &str,
        _broker: &str,
        _topic: &str,
        _partition: i32,
    ) -> Result<(), Errors> {
        self.kafka_attempts += 1;
        if self.should_inject_fault(&FaultType::KafkaConnectionFailure)
            && self.kafka_attempts <= self.kafka_failures
        {
            warn!("Injecting fault for Kafka connection error");
            return Err(Errors::KafkaConnectionError);
        }
        trace!("Not injecting fault for Kafka connection error");
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    async fn connect_to_redis(&mut self, _path: &str) -> Result<(), Errors> {
        if self.should_inject_fault(&FaultType::RedisConnectionFailure) {
            warn!("Injecting fault for Redis connection error");
            return Err(Errors::RedisConnectionError);
        }
        trace!("Not injecting fault for Redis connection error");
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    async fn open_file(&mut self, path: &Path) -> Result<(), Errors> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(path)
            .await
            .map_err(|_| Errors::FileOpenError)?;
        let sim_file = SimulatedFile::new(self.rng.clone(), RealFile { file: Some(file) });
        self.file = Some(sim_file);
        Ok(())
    }

    async fn read_kafka_message(&mut self) -> Result<Option<String>, Errors> {
        if self.should_inject_fault(&FaultType::KafkaReadFailure) {
            warn!("Injecting fault for Kafka read error");
            return Err(Errors::NoKafkaMessage);
        }
        trace!("Not injecting fault for Kafka read error");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(self.kafka_messages.len() > 0);
        if let Some(message) = self.kafka_messages.choose(&mut self.rng) {
            return Ok(Some(message.clone()));
        }
        return Ok(None);
    }

    async fn get_redis_config(&mut self, key: &str) -> Result<String, Errors> {
        trace!("calling get_redis_config");
        if self.should_inject_fault(&FaultType::RedisReadFailure) {
            warn!("Injecting fault for Redis read error");
            return Err(Errors::RedisKeyRetrievalError);
        }
        trace!("Not injecting fault for Redis read error");
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.redis_data
            .get(key)
            .ok_or(Errors::RedisKeyRetrievalError)
            .cloned()
    }

    async fn read_file(&mut self, size: usize) -> Result<Vec<u8>, Errors> {
        self.file.as_mut().unwrap().read(size).await
    }

    async fn write_to_file(&mut self, data: &str) -> Result<usize, Errors> {
        self.file.as_mut().unwrap().write(data).await
    }

    async fn read_last_n_entries(&mut self, n: usize) -> Result<Vec<String>, Errors> {
        self.file.as_mut().unwrap().read_last_n_entries(n).await
    }

    fn generate_jitter(&mut self, base_delay: Duration) -> Duration {
        let jitter: u64 = self.rng.gen_range(0..base_delay.as_millis() as u64);
        base_delay + Duration::from_millis(jitter)
    }

    async fn sleep(&mut self, duration: Duration) {
        self.clock.sleep(duration).await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    info!("Starting application with args: {:?}", args);

    if args.simulate {
        let seed = match std::env::var("SEED") {
            Ok(seed) => seed.parse::<u64>().unwrap(),
            Err(_) => rand::thread_rng().next_u64(),
        };
        info!("Running simulator with seed {}", seed);
        let mut io = SimulatedIO::new(seed);
        start(&mut io).await;
    } else {
        let mut io = RealIO::new();
        start(&mut io).await;
    }
}

async fn start(io: &mut dyn IO) {
    let max_retries = 5;
    let base_delay = Duration::from_millis(10);
    let mut retries = 0;
    let mut delay = base_delay;
    loop {
        match io
            .create_kafka_consumer("group_id", "localhost:9092", "dummy_topic", 0)
            .await
        {
            Ok(_) => break,
            Err(_) if retries < max_retries => {
                retries += 1;
                let delay_with_jitter = io.generate_jitter(delay);
                io.sleep(delay_with_jitter).await;
                delay *= 2;
            }
            Err(err) => {
                eprintln!("failed to create Kafka consumer: {:?}", err);
                return;
            }
        }
    }

    let max_retries = 5;
    let base_delay = Duration::from_millis(10);
    let mut retries = 0;
    let mut delay = base_delay;
    loop {
        match io.connect_to_redis("redis://127.0.0.1").await {
            Ok(_) => break,
            Err(_) if retries < max_retries => {
                retries += 1;
                let delay_with_jitter = io.generate_jitter(delay);
                io.sleep(delay_with_jitter).await;
                delay *= 2;
            }
            Err(err) => {
                eprintln!("failed to create Kafka consumer: {:?}", err);
                return;
            }
        }
    }

    io.open_file(Path::new("output.txt")).await.unwrap();
    run(io).await;
}

async fn run(io: &mut dyn IO) {
    let config_key = "config_key";
    let mut counter = 0;
    let mut written_messages = Vec::new();
    loop {
        counter += 1;
        trace!("Iteration {counter}");

        //  Get Kafka message
        let max_retries = 5;
        let base_delay = Duration::from_millis(10);
        let mut retries = 0;
        let mut delay = base_delay;

        let kafka_message = loop {
            match io.read_kafka_message().await {
                Ok(Some(message)) => break Ok(message),
                Ok(None) => {
                    panic!("Error");
                }
                Err(_) if retries < max_retries => {
                    retries += 1;
                    let delay_with_jitter = io.generate_jitter(delay);
                    io.sleep(delay_with_jitter).await;
                    delay *= 2;
                }
                Err(err) => {
                    error!("failed to read message from Kafka: {:?}", err);
                    break Err(err);
                }
            };

            if retries >= max_retries {
                panic!("failed to read the message from Kafka after all retries",);
            }
        }
        .unwrap();

        //  Get Redis config
        let max_retries = 5;
        let base_delay = Duration::from_millis(10);
        let mut retries = 0;
        let mut delay = base_delay;

        let redis_config = loop {
            match io.get_redis_config(&config_key).await {
                Ok(message) => break Ok(message),
                Err(_) if retries < max_retries => {
                    retries += 1;
                    let delay_with_jitter = io.generate_jitter(delay);
                    io.sleep(delay_with_jitter).await;
                    delay *= 2;
                }
                Err(err) => {
                    error!("failed to read config from Redis: {:?}", err);
                    break Err(err);
                }
            };

            if retries >= max_retries {
                panic!("failed to read the message from Kafka after all retries",);
            }
        }
        .unwrap();
        let output = format!("Config: {}, Message: {}\n", redis_config, kafka_message);

        match io.write_to_file(&output).await {
            Ok(_) => {
                written_messages.push(output.clone());
                if counter % 5 == 0 {
                    match io.read_last_n_entries(5).await {
                        Ok(read_messages) => {
                            let expected = &written_messages[written_messages.len() - 5..];
                            if read_messages != expected {
                                error!(
                                    "Data verification failed! Expected {:?}, got {:?}",
                                    expected, read_messages
                                );
                                panic!("Data verification failed");
                            }
                        }
                        Err(e) => {
                            error!("failed to read last n messages: {:?}", e);
                            panic!("Failed to read back last n messages");
                        }
                    }
                }
            }
            Err(e) => {
                error!("failed to write to file {:?}", e);
            }
        }
    }
}
