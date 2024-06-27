use super::{
    error::Error,
    ftdi::{list_ftdi_devices, FtdiDevice, FtdiError},
};
use serial2::SerialPort;
use std::{
    collections::VecDeque,
    fmt::Display,
    io::{BufReader, BufWriter, ErrorKind, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

pub enum DataType {
    Command,
    Response,
    Packet,
    KeepAlive,
}

impl From<DataType> for u32 {
    fn from(value: DataType) -> Self {
        match value {
            DataType::Command => 1,
            DataType::Response => 2,
            DataType::Packet => 3,
            DataType::KeepAlive => 0xCAFEBEEF,
        }
    }
}

impl TryFrom<u32> for DataType {
    type Error = Error;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Command,
            2 => Self::Response,
            3 => Self::Packet,
            0xCAFEBEEF => Self::KeepAlive,
            _ => return Err(Error::new("Unknown data type")),
        })
    }
}

pub struct Command {
    pub id: u8,
    pub args: [u32; 2],
    pub data: Vec<u8>,
}

pub struct Response {
    pub id: u8,
    pub data: Vec<u8>,
    pub error: bool,
}

pub struct Packet {
    pub id: u8,
    pub data: Vec<u8>,
}

const SERIAL_PREFIX: &str = "serial://";
const FTDI_PREFIX: &str = "ftdi://";

const RESET_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(1);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Backend {
    fn reset(&mut self) -> Result<(), Error>;

    fn close(&self);

    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize>;

    fn write(&mut self, buffer: &[u8]) -> std::io::Result<()>;

    fn flush(&mut self) -> std::io::Result<()>;

    fn purge_incoming_data(&mut self) -> std::io::Result<()> {
        let timeout = Instant::now();
        loop {
            match self.read(&mut vec![0; 1]) {
                Ok(length) => match length {
                    0 => return Ok(()),
                    _ => {}
                },
                Err(error) => match error.kind() {
                    ErrorKind::TimedOut => return Ok(()),
                    _ => return Err(error),
                },
            }
            if timeout.elapsed() >= RESET_TIMEOUT {
                return Err(std::io::Error::new(
                    ErrorKind::TimedOut,
                    "SC64 read buffer flush took too long",
                ));
            }
        }
    }

    fn try_read(&mut self, buffer: &mut [u8], block: bool) -> Result<Option<()>, Error> {
        let mut position = 0;
        let length = buffer.len();
        let timeout = Instant::now();
        while position < length {
            match self.read(&mut buffer[position..length]) {
                Ok(0) => return Err(Error::new("Unexpected end of stream data")),
                Ok(bytes) => position += bytes,
                Err(error) => match error.kind() {
                    ErrorKind::Interrupted | ErrorKind::TimedOut | ErrorKind::WouldBlock => {
                        if !block && position == 0 {
                            return Ok(None);
                        }
                    }
                    _ => return Err(error.into()),
                },
            }
            if timeout.elapsed() > READ_TIMEOUT {
                return Err(Error::new("Read timeout"));
            }
        }
        Ok(Some(()))
    }

    fn try_read_header(&mut self, block: bool) -> Result<Option<[u8; 4]>, Error> {
        let mut header = [0u8; 4];
        Ok(self.try_read(&mut header, block)?.map(|_| header))
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        match self.try_read(buffer, true)? {
            Some(()) => Ok(()),
            None => Err(Error::new("Unexpected end of data")),
        }
    }

    fn send_command(&mut self, command: &Command) -> Result<(), Error> {
        self.write(b"CMD")?;
        self.write(&command.id.to_be_bytes())?;

        self.write(&command.args[0].to_be_bytes())?;
        self.write(&command.args[1].to_be_bytes())?;

        self.write(&command.data)?;

        self.flush()?;

        Ok(())
    }

    fn process_incoming_data(
        &mut self,
        data_type: DataType,
        packets: &mut VecDeque<Packet>,
    ) -> Result<Option<Response>, Error> {
        let block = matches!(data_type, DataType::Response);

        while let Some(header) = self.try_read_header(block)? {
            let (packet_token, error) = (match &header[0..3] {
                b"CMP" => Ok((false, false)),
                b"PKT" => Ok((true, false)),
                b"ERR" => Ok((false, true)),
                _ => Err(Error::new("Unknown response token")),
            })?;
            let id = header[3];

            let mut buffer = [0u8; 4];

            self.read_exact(&mut buffer)?;
            let length = u32::from_be_bytes(buffer) as usize;

            let mut data = vec![0u8; length];
            self.read_exact(&mut data)?;

            if packet_token {
                packets.push_back(Packet { id, data });
                if matches!(data_type, DataType::Packet) {
                    break;
                }
            } else {
                return Ok(Some(Response { id, error, data }));
            }
        }

        Ok(None)
    }
}

pub struct SerialBackend {
    device: SerialPort,
}

impl Backend for SerialBackend {
    fn reset(&mut self) -> Result<(), Error> {
        self.device.set_dtr(true)?;
        let timeout = Instant::now();
        loop {
            self.device.discard_buffers()?;
            if self.device.read_dsr()? {
                break;
            }
            if timeout.elapsed() > RESET_TIMEOUT {
                return Err(Error::new("Couldn't reset SC64 device (on)"));
            }
        }

        self.purge_incoming_data()?;

        self.device.set_dtr(false)?;
        let timeout = Instant::now();
        loop {
            if !self.device.read_dsr()? {
                break;
            }
            if timeout.elapsed() > RESET_TIMEOUT {
                return Err(Error::new("Couldn't reset SC64 device (off)"));
            }
        }

        Ok(())
    }

    fn close(&self) {}

    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.device.read(buffer)
    }

    fn write(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.device.write_all(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.device.flush()
    }
}

fn new_serial_backend(port: &str) -> std::io::Result<SerialBackend> {
    let mut serial = SerialPort::open(port, 115_200)?;
    serial.set_read_timeout(POLL_TIMEOUT)?;
    serial.set_write_timeout(WRITE_TIMEOUT)?;
    Ok(SerialBackend { device: serial })
}

struct FtdiBackend {
    device: FtdiDevice,
}

impl Backend for FtdiBackend {
    fn reset(&mut self) -> Result<(), Error> {
        self.device.set_dtr(true)?;
        let timeout = Instant::now();
        loop {
            self.device.discard_buffers()?;
            if self.device.read_dsr()? {
                break;
            }
            if timeout.elapsed() > RESET_TIMEOUT {
                return Err(Error::new("Couldn't reset SC64 device (on)"));
            }
        }

        self.purge_incoming_data()?;

        self.device.set_dtr(false)?;
        let timeout = Instant::now();
        loop {
            if !self.device.read_dsr()? {
                break;
            }
            if timeout.elapsed() > RESET_TIMEOUT {
                return Err(Error::new("Couldn't reset SC64 device (off)"));
            }
        }

        Ok(())
    }

    fn close(&self) {}

    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.device.read(buffer)
    }

    fn write(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.device.write_all(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn new_ftdi_backend(port: &str) -> Result<FtdiBackend, FtdiError> {
    Ok(FtdiBackend {
        device: FtdiDevice::open(port, POLL_TIMEOUT, WRITE_TIMEOUT)?,
    })
}

struct TcpBackend {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
}

impl Backend for TcpBackend {
    fn reset(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn close(&self) {
        self.stream.shutdown(std::net::Shutdown::Both).ok();
    }

    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buffer)
    }

    fn write(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    fn send_command(&mut self, command: &Command) -> Result<(), Error> {
        let payload_data_type: u32 = DataType::Command.into();
        self.write(&payload_data_type.to_be_bytes())?;

        self.write(&command.id.to_be_bytes())?;
        self.write(&command.args[0].to_be_bytes())?;
        self.write(&command.args[1].to_be_bytes())?;

        let command_data_length = command.data.len() as u32;
        self.write(&command_data_length.to_be_bytes())?;
        self.write(&command.data)?;

        self.flush()?;

        Ok(())
    }

    fn process_incoming_data(
        &mut self,
        data_type: DataType,
        packets: &mut VecDeque<Packet>,
    ) -> Result<Option<Response>, Error> {
        let block = matches!(data_type, DataType::Response);
        while let Some(header) = self.try_read_header(block)? {
            let payload_data_type: DataType = u32::from_be_bytes(header).try_into()?;
            let mut buffer = [0u8; 4];
            match payload_data_type {
                DataType::Response => {
                    let mut response_info = vec![0u8; 2];
                    self.read_exact(&mut response_info)?;

                    self.read_exact(&mut buffer)?;
                    let response_data_length = u32::from_be_bytes(buffer) as usize;

                    let mut data = vec![0u8; response_data_length];
                    self.read_exact(&mut data)?;

                    return Ok(Some(Response {
                        id: response_info[0],
                        error: response_info[1] != 0,
                        data,
                    }));
                }
                DataType::Packet => {
                    let mut packet_info = vec![0u8; 1];
                    self.read_exact(&mut packet_info)?;

                    self.read_exact(&mut buffer)?;
                    let packet_data_length = u32::from_be_bytes(buffer) as usize;

                    let mut data = vec![0u8; packet_data_length];
                    self.read_exact(&mut data)?;

                    packets.push_back(Packet {
                        id: packet_info[0],
                        data,
                    });
                    if matches!(data_type, DataType::Packet) {
                        break;
                    }
                }
                DataType::KeepAlive => {}
                _ => return Err(Error::new("Unexpected payload data type received")),
            };
        }

        Ok(None)
    }
}

fn new_tcp_backend(address: &str) -> Result<TcpBackend, Error> {
    let stream = match TcpStream::connect(address) {
        Ok(stream) => {
            stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
            stream.set_read_timeout(Some(POLL_TIMEOUT))?;
            stream
        }
        Err(error) => {
            return Err(Error::new(
                format!("Couldn't connect to [{address}]: {error}").as_str(),
            ))
        }
    };
    let reader = BufReader::new(stream.try_clone()?);
    let writer = BufWriter::new(stream.try_clone()?);
    Ok(TcpBackend {
        stream,
        reader,
        writer,
    })
}

fn new_local_backend(port: &str) -> Result<Box<dyn Backend>, Error> {
    let mut backend: Box<dyn Backend> = if port.starts_with(SERIAL_PREFIX) {
        Box::new(new_serial_backend(
            port.strip_prefix(SERIAL_PREFIX).unwrap_or_default(),
        )?)
    } else if port.starts_with(FTDI_PREFIX) {
        Box::new(new_ftdi_backend(
            port.strip_prefix(FTDI_PREFIX).unwrap_or_default(),
        )?)
    } else {
        return Err(Error::new("Invalid port prefix provided"));
    };
    backend.reset()?;
    Ok(backend)
}

fn new_remote_backend(address: &str) -> Result<Box<dyn Backend>, Error> {
    Ok(Box::new(new_tcp_backend(address)?))
}

pub struct Link {
    pub backend: Box<dyn Backend>,
    packets: VecDeque<Packet>,
}

impl Link {
    pub fn execute_command(&mut self, command: &Command) -> Result<Vec<u8>, Error> {
        self.execute_command_raw(command, false, false)
    }

    pub fn execute_command_raw(
        &mut self,
        command: &Command,
        no_response: bool,
        ignore_error: bool,
    ) -> Result<Vec<u8>, Error> {
        self.backend.send_command(command)?;
        if no_response {
            return Ok(vec![]);
        }
        let response = self.receive_response()?;
        if command.id != response.id {
            return Err(Error::new("Command response ID didn't match"));
        }
        if !ignore_error && response.error {
            return Err(Error::new("Command response error"));
        }
        Ok(response.data)
    }

    fn receive_response(&mut self) -> Result<Response, Error> {
        match self
            .backend
            .process_incoming_data(DataType::Response, &mut self.packets)
        {
            Ok(response) => match response {
                Some(response) => Ok(response),
                None => Err(Error::new("No response was received")),
            },
            Err(error) => Err(Error::new(
                format!("Command response error: {error}").as_str(),
            )),
        }
    }

    pub fn receive_packet(&mut self) -> Result<Option<Packet>, Error> {
        if self.packets.len() == 0 {
            let response = self
                .backend
                .process_incoming_data(DataType::Packet, &mut self.packets)?;
            if response.is_some() {
                return Err(Error::new("Unexpected command response in data stream"));
            }
        }
        Ok(self.packets.pop_front())
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.backend.close();
    }
}

pub fn new_local(port: &str) -> Result<Link, Error> {
    Ok(Link {
        backend: new_local_backend(port)?,
        packets: VecDeque::new(),
    })
}

pub fn new_remote(address: &str) -> Result<Link, Error> {
    Ok(Link {
        backend: new_remote_backend(address)?,
        packets: VecDeque::new(),
    })
}

pub enum BackendType {
    Serial,
    Ftdi,
}

impl Display for BackendType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Serial => "serial",
            Self::Ftdi => "libftdi",
        })
    }
}

pub struct DeviceInfo {
    pub backend: BackendType,
    pub port: String,
    pub serial: String,
}

pub fn list_local_devices() -> Result<Vec<DeviceInfo>, Error> {
    const SC64_VID: u16 = 0x0403;
    const SC64_PID: u16 = 0x6014;
    const SC64_SID: &str = "SC64";

    let mut devices: Vec<DeviceInfo> = Vec::new();

    if let Ok(list) = list_ftdi_devices(SC64_VID, SC64_PID) {
        for device in list.into_iter() {
            if device.serial.starts_with(SC64_SID) {
                devices.push(DeviceInfo {
                    backend: BackendType::Ftdi,
                    port: format!("{FTDI_PREFIX}{}", device.port),
                    serial: device.serial,
                })
            }
        }
    }

    if let Ok(list) = serialport::available_ports() {
        for device in list.into_iter() {
            if let serialport::SerialPortType::UsbPort(i) = device.port_type {
                if let Some(serial) = i.serial_number {
                    if i.vid == SC64_VID && i.pid == SC64_PID && serial.starts_with(SC64_SID) {
                        devices.push(DeviceInfo {
                            backend: BackendType::Serial,
                            port: format!("{SERIAL_PREFIX}{}", device.port_name),
                            serial,
                        });
                    }
                }
            }
        }
    }

    if devices.len() == 0 {
        return Err(Error::new("No SC64 devices found"));
    }

    return Ok(devices);
}
