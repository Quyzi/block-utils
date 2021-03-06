extern crate fstab;
extern crate libudev;
#[macro_use]
extern crate log;
#[macro_use]
extern crate nom;
extern crate regex;
extern crate shellscript;
extern crate uuid;

pub mod nvme;

use fstab::{FsEntry, FsTab};
use nom::is_digit;
use regex::Regex;
use uuid::Uuid;

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{BufReader, BufRead, Read, Write};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::str::{from_utf8, FromStr};

// Formats a block device at Path p with XFS
/// This is used for formatting btrfs filesystems and setting the metadata profile
#[derive(Clone, Debug)]
pub enum MetadataProfile {
    Raid0,
    Raid1,
    Raid5,
    Raid6,
    Raid10,
    Single,
    Dup,
}

/// What raid card if any the system is using to serve disks
#[derive(Clone, Debug)]
pub enum RaidType {
    None,
    Cisco,
    Hp,
    Lsi,
}
impl FromStr for RaidType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ATA" => Ok(RaidType::None),
            "CISCO" => Ok(RaidType::Cisco),
            "HP" => Ok(RaidType::Hp),
            "HPE" => Ok(RaidType::Hp),
            "LSI" => Ok(RaidType::Lsi),
            _ => Err(format!("Unknown Raid Vendor: {}", s)),
        }
    }
}

// This will be used to make intelligent decisions about setting up the device
/// Device information that is gathered with udev
#[derive(Clone, Debug)]
pub struct Device {
    pub id: Option<Uuid>,
    pub name: String,
    pub media_type: MediaType,
    pub capacity: u64,
    pub fs_type: FilesystemType,
    pub serial_number: Option<String>,
}

#[derive(Debug)]
pub struct AsyncInit {
    /// The child process needed for this device initializati
    /// This will be an async spawned Child handle
    pub format_child: Child,
    /// After formatting is complete run these commands to se
    /// ZFS needs this.  These should prob be run in sync mod
    pub post_setup_commands: Vec<(String, Vec<String>)>,
    /// The device we're initializing
    pub device: PathBuf,
}

#[derive(Debug)]
pub enum Scheduler {
    /// Try to balance latency and throughput
    Cfq,
    /// Latency is most important
    Deadline,
    /// Throughput is most important
    Noop,
}

impl fmt::Display for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            &Scheduler::Cfq => "cfq",
            &Scheduler::Deadline => "deadline",
            &Scheduler::Noop => "noop",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for Scheduler {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cfq" => Ok(Scheduler::Cfq),
            "deadline" => Ok(Scheduler::Deadline),
            "noop" => Ok(Scheduler::Noop),
            _ => Err(format!("Unknown scheduler {}", s)),
        }
    }
}

/// What type of media has been detected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaType {
    /// AKA SSD
    SolidState,
    /// Regular rotational device
    Rotational,
    /// Special loopback device
    Loopback,
    // Logical volume device
    LVM,
    // Software raid device
    MdRaid,
    // NVM Express
    NVME,
    // Ramdisk
    Ram,
    Virtual,
    Unknown,
}

/// What type of filesystem
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilesystemType {
    Btrfs,
    Ext2,
    Ext3,
    Ext4,
    Xfs,
    Zfs,
    Unknown,
}

impl FilesystemType {
    pub fn from_str(fs_type: &str) -> FilesystemType {
        match fs_type {
            "btrfs" => FilesystemType::Btrfs,
            "ext2" => FilesystemType::Ext2,
            "ext3" => FilesystemType::Ext3,
            "ext4" => FilesystemType::Ext4,
            "xfs" => FilesystemType::Xfs,
            "zfs" => FilesystemType::Zfs,
            _ => FilesystemType::Unknown,
        }
    }
    pub fn to_str(&self) -> &str {
        match self {
            &FilesystemType::Btrfs => "btrfs",
            &FilesystemType::Ext2 => "ext2",
            &FilesystemType::Ext3 => "ext3",
            &FilesystemType::Ext4 => "ext4",
            &FilesystemType::Xfs => "xfs",
            &FilesystemType::Zfs => "zfs",
            &FilesystemType::Unknown => "unknown",
        }
    }
    pub fn to_string(&self) -> String {
        match self {
            &FilesystemType::Btrfs => "btrfs".to_string(),
            &FilesystemType::Ext2 => "ext2".to_string(),
            &FilesystemType::Ext3 => "ext3".to_string(),
            &FilesystemType::Ext4 => "ext4".to_string(),
            &FilesystemType::Xfs => "xfs".to_string(),
            &FilesystemType::Zfs => "zfs".to_string(),
            &FilesystemType::Unknown => "unknown".to_string(),
        }
    }
}

impl MetadataProfile {
    pub fn to_string(self) -> String {
        match self {
            MetadataProfile::Raid0 => "raid0".to_string(),
            MetadataProfile::Raid1 => "raid1".to_string(),
            MetadataProfile::Raid5 => "raid5".to_string(),
            MetadataProfile::Raid6 => "raid6".to_string(),
            MetadataProfile::Raid10 => "raid10".to_string(),
            MetadataProfile::Single => "single".to_string(),
            MetadataProfile::Dup => "dup".to_string(),
        }
    }
}

/// This allows you to tweak some settings when you're formatting the filesystem
#[derive(Debug)]
pub enum Filesystem {
    Btrfs {
        leaf_size: u64,
        metadata_profile: MetadataProfile,
        node_size: u64,
    },
    Ext4 {
        inode_size: u64,
        reserved_blocks_percentage: u8,
        stride: Option<u64>,
        stripe_width: Option<u64>,
    },
    Xfs {
        /// This is optional.  Boost knobs are on by default:
        /// http://xfs.org/index.php/XFS_FAQ#Q:_I_want_to_tune_my_XFS_filesystems_
        /// for_.3Csomething.3E
        block_size: Option<u64>, // Note this MUST be a power of 2
        force: bool,
        inode_size: Option<u64>,
        stripe_size: Option<u64>, // RAID controllers stripe
        stripe_width: Option<u64>, // IE # of data disks
    },
    Zfs {
        /// The default blocksize for volumes is 8 Kbytes. An
        /// power of 2 from 512 bytes to 128 Kbytes is valid.
        block_size: Option<u64>,
        /// Enable compression on the volume. Default is fals
        compression: Option<bool>,
    },
}


impl Filesystem {
    pub fn new(name: &str) -> Filesystem {
        match name.trim() {
            // Defaults.  Can be changed as needed by the caller
            "zfs" => {
                Filesystem::Zfs {
                    block_size: None,
                    compression: None,
                }
            }
            "xfs" => {
                Filesystem::Xfs {
                    stripe_size: None,
                    stripe_width: None,
                    block_size: None,
                    inode_size: Some(512),
                    force: false,
                }
            }
            "btrfs" => {
                Filesystem::Btrfs {
                    metadata_profile: MetadataProfile::Single,
                    leaf_size: 32768,
                    node_size: 32768,
                }
            }
            "ext4" => {
                Filesystem::Ext4 {
                    inode_size: 512,
                    reserved_blocks_percentage: 0,
                    stride: None,
                    stripe_width: None,
                }
            }
            _ => {
                Filesystem::Xfs {
                    stripe_size: None,
                    stripe_width: None,
                    block_size: None,
                    inode_size: None,
                    force: false,
                }
            }
        }
    }
}

fn run_command<S: AsRef<OsStr>>(command: &str, arg_list: &[S]) -> Output {
    let mut cmd = Command::new(command);
    cmd.args(arg_list);
    let output = cmd.output().unwrap_or_else(
        |e| panic!("failed to execute process: {} ", e),
    );
    return output;
}

/// Utility function to mount a device at a mount point
/// NOTE: This assumes the device is formatted at this point.  The mount
/// will fail if the device isn't formatted.
pub fn mount_device(device: &Device, mount_point: &str) -> Result<i32, String> {
    let mut arg_list: Vec<String> = Vec::new();
    match device.id {
        Some(id) => {
            arg_list.push("-U".to_string());
            arg_list.push(id.hyphenated().to_string());
        }
        None => {
            arg_list.push(format!("/dev/{}", device.name));
        }
    };
    arg_list.push(mount_point.to_string());
    debug!("mount: {:?}", arg_list);

    return process_output(run_command("mount", &arg_list));
}

//Utility function to unmount a device at a mount point
pub fn unmount_device(mount_point: &str) -> Result<i32, String> {
    let mut arg_list: Vec<String> = Vec::new();
    arg_list.push(mount_point.to_string());

    return process_output(run_command("umount", &arg_list));
}

/// Parse mtab and return the device which is mounted at a given directory
pub fn get_mount_device(mount_dir: &Path) -> io::Result<Option<PathBuf>> {
    let dir = mount_dir.to_string_lossy().into_owned();
    let f = fs::File::open("/etc/mtab")?;
    let reader = BufReader::new(f);

    for line in reader.lines() {
        let l = line?;
        if l.contains(&dir) {
            let parts: Vec<&str> = l.split_whitespace().collect();
            if parts.len() > 0 {
                return Ok(Some(PathBuf::from(parts[0])));
            }
        }
    }
    Ok(None)
}

/// Parse mtab and return all mounted block devices not including LVM
pub fn get_mounted_devices() -> io::Result<Vec<Device>> {
    let mut results: Vec<Device> = Vec::new();

    let mtab_devices: Vec<FsEntry> = FsTab::new(Path::new("/etc/mtab"))
        .get_entries()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
        .into_iter()
        .filter(|d| d.fs_spec.contains("/dev/"))
        .filter(|d| !d.fs_spec.contains("mapper"))
        .collect();
    for d in mtab_devices {
        results.push(Device {
            id: None,
            name: Path::new(&d.fs_spec)
                .file_name()
                .unwrap_or(OsStr::new(""))
                .to_string_lossy()
                .into_owned(),
            media_type: MediaType::Unknown,
            capacity: 0,
            fs_type: FilesystemType::from_str(&d.vfs_type),
            serial_number: None,
        });
    }

    Ok(results)
}

/// Parse mtab and return the mountpoint the device is mounted at.
/// This is the opposite of get_mount_device
pub fn get_mountpoint(device: &Path) -> io::Result<Option<PathBuf>> {
    let s = device.to_string_lossy().into_owned();
    let f = fs::File::open("/etc/mtab")?;
    let reader = BufReader::new(f);

    for line in reader.lines() {
        let l = line?;
        if l.contains(&s) {
            let parts: Vec<&str> = l.split_whitespace().collect();
            if parts.len() > 0 {
                return Ok(Some(PathBuf::from(parts[1])));
            }
        }
    }
    Ok(None)
}

fn process_output(output: Output) -> Result<i32, String> {
    if output.status.success() {
        Ok(0)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Err(stderr)
    }
}

pub fn erase_block_device(device: &Path) -> Result<(), String> {
    let output = Command::new("sgdisk")
        .args(&["--zap", &device.to_string_lossy()])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Disk {:?} failed to erase: {}",
            device,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Synchronous utility to format a block device with a given filesystem.
/// Note: ZFS creation can be slow because there's potentially several commands that need to
/// be run.  async_format_block_device will be faster if you have many block devices to format
pub fn format_block_device(device: &Path, filesystem: &Filesystem) -> Result<i32, String> {
    match filesystem {
        &Filesystem::Btrfs {
            ref metadata_profile,
            ref leaf_size,
            ref node_size,
        } => {
            let mut arg_list: Vec<String> = Vec::new();

            arg_list.push("-m".to_string());
            arg_list.push(metadata_profile.clone().to_string());

            arg_list.push("-l".to_string());
            arg_list.push(leaf_size.to_string());

            arg_list.push("-n".to_string());
            arg_list.push(node_size.to_string());

            arg_list.push(device.to_string_lossy().to_string());
            // Check if mkfs.btrfs is installed
            if !Path::new("/sbin/mkfs.btrfs").exists() {
                return Err("Please install btrfs-tools".into());
            }
            return process_output(run_command("mkfs.btrfs", &arg_list));
        }
        &Filesystem::Xfs {
            ref inode_size,
            ref force,
            ref block_size,
            ref stripe_size,
            ref stripe_width,
        } => {
            let mut arg_list: Vec<String> = Vec::new();

            if (*inode_size).is_some() {
                arg_list.push("-i".to_string());
                arg_list.push(format!("size={}", inode_size.unwrap()));
            }

            if *force {
                arg_list.push("-f".to_string());
            }

            arg_list.push(device.to_string_lossy().to_string());

            // Check if mkfs.xfs is installed
            if !Path::new("/sbin/mkfs.xfs").exists() {
                return Err("Please install xfsprogs".into());
            }
            return process_output(run_command("/sbin/mkfs.xfs", &arg_list));
        }
        &Filesystem::Ext4 {
            ref inode_size,
            ref reserved_blocks_percentage,
            ref stride,
            ref stripe_width,
        } => {
            let mut arg_list: Vec<String> = Vec::new();

            arg_list.push("-I".to_string());
            arg_list.push(inode_size.to_string());

            arg_list.push("-m".to_string());
            arg_list.push(reserved_blocks_percentage.to_string());

            arg_list.push(device.to_string_lossy().to_string());

            return process_output(run_command("mkfs.ext4", &arg_list));
        }
        &Filesystem::Zfs {
            ref block_size,
            ref compression,
        } => {
            // Check if zfs is installed
            if !Path::new("/sbin/zfs").exists() {
                return Err("Please install zfsutils-linux".into());
            }
            let base_name = device.file_name();
            match base_name {
                Some(name) => {
                    //Mount at /mnt/{dev_name}
                    let arg_list: Vec<String> =
                        vec![
                            "create".to_string(),
                            "-f".to_string(),
                            "-m".to_string(),
                            format!("/mnt/{}", name.to_string_lossy().into_owned()),
                            name.to_string_lossy().into_owned(),
                            device.to_string_lossy().into_owned(),
                        ];
                    // Create the zpool
                    let _ = process_output(run_command("/sbin/zpool", &arg_list))?;
                    if block_size.is_some() {
                        // If zpool creation is successful then we set these
                        let _ = process_output(run_command(
                            "/sbin/zfs",
                            &vec![
                                "set".to_string(),
                                format!("recordsize={}", block_size.unwrap()),
                                name.to_string_lossy().into_owned(),
                            ],
                        ))?;
                    }
                    if compression.is_some() {
                        let _ = process_output(run_command(
                            "/sbin/zfs",
                            &vec![
                                "set".to_string(),
                                "compression=on".to_string(),
                                name.to_string_lossy().into_owned(),
                            ],
                        ))?;
                    }
                    let _ = process_output(run_command(
                        "/sbin/zfs",
                        &vec![
                            "set".to_string(),
                            "acltype=posixacl".to_string(),
                            name.to_string_lossy().into_owned(),
                        ],
                    ))?;
                    let _ = process_output(run_command(
                        "/sbin/zfs",
                        &vec![
                            "set".to_string(),
                            "atime=off".to_string(),
                            name.to_string_lossy().into_owned(),
                        ],
                    ))?;
                    return Ok(0);
                }
                None => Err(format!(
                    "Unable to determine filename for device: {:?}",
                    device
                )),
            }
        }

    }
}
pub fn async_format_block_device(
    device: &PathBuf,
    filesystem: &Filesystem,
) -> Result<AsyncInit, String> {
    match filesystem {
        &Filesystem::Btrfs {
            ref metadata_profile,
            ref leaf_size,
            ref node_size,
        } => {
            let arg_list: Vec<String> = vec![
                "-m".to_string(),
                metadata_profile.clone().to_string(),
                "-l".to_string(),
                leaf_size.to_string(),
                "-n".to_string(),
                node_size.to_string(),
                device.to_string_lossy().to_string(),
            ];
            // Check if mkfs.btrfs is installed
            if !Path::new("/sbin/mkfs.btrfs").exists() {
                return Err("Please install btrfs-tools".into());
            }
            return Ok(AsyncInit {
                format_child: Command::new("mkfs.btrfs").args(&arg_list).spawn().map_err(
                    |e| {
                        e.to_string()
                    },
                )?,
                post_setup_commands: vec![],
                device: device.clone(),
            });
        }
        &Filesystem::Xfs {
            ref block_size,
            ref inode_size,
            ref stripe_size,
            ref stripe_width,
            ref force,
        } => {
            let mut arg_list: Vec<String> = Vec::new();

            if (*inode_size).is_some() {
                arg_list.push("-i".to_string());
                arg_list.push(format!("size={}", inode_size.unwrap()));
            }

            if *force {
                arg_list.push("-f".to_string());
            }

            if (*stripe_size).is_some() && (*stripe_width).is_some() {
                arg_list.push("-d".to_string());
                arg_list.push(format!("su={}", stripe_size.unwrap()));
                arg_list.push(format!("sw={}", stripe_width.unwrap()));
            }

            arg_list.push(device.to_string_lossy().to_string());

            // Check if mkfs.xfs is installed
            if !Path::new("/sbin/mkfs.xfs").exists() {
                return Err("Please install xfsprogs".into());
            }
            let format_handle = Command::new("/sbin/mkfs.xfs")
                .args(&arg_list)
                .spawn()
                .map_err(|e| e.to_string())?;
            return Ok(AsyncInit {
                format_child: format_handle,
                post_setup_commands: vec![],
                device: device.clone(),
            });
        }
        &Filesystem::Zfs {
            ref block_size,
            ref compression,
        } => {
            // Check if zfs is installed
            if !Path::new("/sbin/zfs").exists() {
                return Err("Please install zfsutils-linux".into());
            }
            let base_name = device.file_name();
            match base_name {
                Some(name) => {
                    //Mount at /mnt/{dev_name}
                    let mut post_setup_commands: Vec<(String, Vec<String>)> = Vec::new();
                    let arg_list: Vec<String> =
                        vec![
                            "create".to_string(),
                            "-f".to_string(),
                            "-m".to_string(),
                            format!("/mnt/{}", name.to_string_lossy().into_owned()),
                            name.to_string_lossy().into_owned(),
                            device.to_string_lossy().into_owned(),
                        ];
                    let zpool_create = Command::new("/sbin/zpool")
                        .args(&arg_list)
                        .spawn()
                        .map_err(|e| e.to_string())?;

                    if block_size.is_some() {
                        // If zpool creation is successful then we set these
                        post_setup_commands.push((
                            "/sbin/zfs".to_string(),
                            vec![
                                "set".to_string(),
                                format!("recordsize={}", block_size.unwrap()),
                                name.to_string_lossy().into_owned(),
                            ],
                        ));
                    }
                    if compression.is_some() {
                        post_setup_commands.push((
                            "/sbin/zfs".to_string(),
                            vec![
                                "set".to_string(),
                                "compression=on".to_string(),
                                name.to_string_lossy().into_owned(),
                            ],
                        ));
                    }
                    post_setup_commands.push((
                        "/sbin/zfs".to_string(),
                        vec![
                            "set".to_string(),
                            "acltype=posixacl".to_string(),
                            name.to_string_lossy().into_owned(),
                        ],
                    ));
                    post_setup_commands.push((
                        "/sbin/zfs".to_string(),
                        vec![
                            "set".to_string(),
                            "atime=off".to_string(),
                            name.to_string_lossy().into_owned(),
                        ],
                    ));
                    return Ok(AsyncInit {
                        format_child: zpool_create,
                        post_setup_commands: post_setup_commands,
                        device: device.clone(),
                    });
                }
                None => Err(format!(
                    "Unable to determine filename for device: {:?}",
                    device
                )),
            }
        }
        &Filesystem::Ext4 {
            ref inode_size,
            ref reserved_blocks_percentage,
            ref stride,
            ref stripe_width,
        } => {
            let mut arg_list: Vec<String> =
                vec!["-m".to_string(), reserved_blocks_percentage.to_string()];

            arg_list.push("-I".to_string());
            arg_list.push(inode_size.to_string());

            if (*stride).is_some() {
                arg_list.push("-E".to_string());
                arg_list.push(format!("stride={}", stride.unwrap()));
            }
            if (*stripe_width).is_some() {
                arg_list.push("-E".to_string());
                arg_list.push(format!("stripe_width={}", stripe_width.unwrap()));
            }
            arg_list.push(device.to_string_lossy().into_owned());

            return Ok(AsyncInit {
                format_child: Command::new("mkfs.ext4").args(&arg_list).spawn().map_err(
                    |e| {
                        e.to_string()
                    },
                )?,
                post_setup_commands: vec![],
                device: device.clone(),
            });
        }
    }
}

#[test]
fn test_get_device_info() {
    print!("{:?}", get_device_info(&PathBuf::from("/dev/sda1")));
    print!("{:?}", get_device_info(&PathBuf::from("/dev/loop0")));
}

fn get_size(device: &libudev::Device) -> Option<u64> {
    match device.attribute_value("size") {
        // 512 is the block size
        Some(size_str) => {
            let size = size_str.to_str().unwrap_or("0").parse::<u64>().unwrap_or(0);
            return Some(size * 512);
        }
        None => return None,
    }
}

fn get_uuid(device: &libudev::Device) -> Option<Uuid> {
    match device.property_value("ID_FS_UUID") {
        Some(value) => {
            match Uuid::parse_str(value.to_str().unwrap()) {
                Ok(result) => return Some(result),
                Err(_) => return None,
            }
        }
        None => return None,
    }
}

fn get_serial(device: &libudev::Device) -> Option<String> {
    match device.property_value("ID_SERIAL") {
        Some(value) => Some(value.to_string_lossy().into_owned()),
        None => None,
    }
}

fn get_fs_type(device: &libudev::Device) -> FilesystemType {
    match device.property_value("ID_FS_TYPE") {
        Some(s) => {
            let value = s.to_string_lossy();
            match value.as_ref() {
                "btrfs" => return FilesystemType::Btrfs,
                "xfs" => return FilesystemType::Xfs,
                "ext4" => return FilesystemType::Ext4,
                _ => return FilesystemType::Unknown,
            }
        }
        None => return FilesystemType::Unknown,
    }
}

fn get_media_type(device: &libudev::Device) -> MediaType {
    let device_sysname = device.sysname().to_string_lossy();

    // Test for loopback
    if let Ok(loop_regex) = Regex::new(r"loop\d+") {
        if loop_regex.is_match(&device_sysname) {
            return MediaType::Loopback;
        }
    }

    // Test for ramdisk
    if let Ok(ramdisk_regex) = Regex::new(r"ram\d+") {
        if ramdisk_regex.is_match(&device_sysname) {
            return MediaType::Ram;
        }
    }

    // Test for software raid
    if let Ok(ramdisk_regex) = Regex::new(r"md\d+") {
        if ramdisk_regex.is_match(&device_sysname) {
            return MediaType::MdRaid;
        }
    }

    // Test for nvme
    if device_sysname.contains("nvme") {
        return MediaType::NVME;
    }

    // Test for LVM
    if let Some(_) = device.property_value("DM_NAME") {
        return MediaType::LVM;
    }

    // That should take care of the tricky ones.  Lets try to identify if it's
    // SSD or rotational now
    if let Some(rotation) = device.property_value("ID_ATA_ROTATION_RATE_RPM") {
        if rotation == "0" {
            return MediaType::SolidState;
        } else {
            return MediaType::Rotational;
        }
    }

    // No rotation rate.  Lets see if it's a virtual qemu disk
    if let Some(vendor) = device.property_value("ID_VENDOR") {
        let value = vendor.to_string_lossy();
        match value.as_ref() {
            "QEMU" => return MediaType::Virtual,
            _ => return MediaType::Unknown,
        }
    }

    // I give up
    return MediaType::Unknown;
}

/// Checks and returns if a particular directory is a mountpoint
pub fn is_mounted(directory: &Path) -> Result<bool, String> {
    let parent = directory.parent();

    let dir_metadata = try!(fs::metadata(directory).map_err(|e| e.to_string()));
    let file_type = dir_metadata.file_type();

    if file_type.is_symlink() {
        // A symlink can never be a mount point
        return Ok(false);
    }

    if parent.is_some() {
        let parent_metadata = try!(fs::metadata(parent.unwrap()).map_err(|e| e.to_string()));
        if parent_metadata.dev() != dir_metadata.dev() {
            // path/.. on a different device as path
            return Ok(true);
        }
    } else {
        // If the directory doesn't have a parent it's the root filesystem
        return Ok(false);
    }
    return Ok(false);
}

/// Scan a system and return all block devices that udev knows about
/// This function will skip udev devices identified as partition.  If
/// it can't discover this it will error on the side of caution and
/// return the device
pub fn get_block_devices() -> Result<Vec<PathBuf>, String> {
    let mut block_devices: Vec<PathBuf> = Vec::new();
    let context = try!(libudev::Context::new().map_err(|e| e.to_string()));
    let mut enumerator = try!(libudev::Enumerator::new(&context).map_err(
        |e| e.to_string(),
    ));
    let devices = try!(enumerator.scan_devices().map_err(|e| e.to_string()));

    for device in devices {
        if device.subsystem() == "block" {
            let partition = match device.devtype() {
                Some(devtype) => if devtype == "partition" { true } else { false },
                None => false,
            };
            if !partition {
                let mut path = PathBuf::from("/dev");
                path.push(device.sysname());
                block_devices.push(path);
            }
        }
    }

    Ok(block_devices)
}

/// Checks to see if the subsystem this device is using is block
pub fn is_block_device(device_path: &PathBuf) -> Result<bool, String> {
    let context = try!(libudev::Context::new().map_err(|e| e.to_string()));
    let mut enumerator = try!(libudev::Enumerator::new(&context).map_err(
        |e| e.to_string(),
    ));
    let devices = try!(enumerator.scan_devices().map_err(|e| e.to_string()));

    let sysname = try!(device_path.file_name().ok_or(format!(
        "Unable to get file_name on device {:?}",
        device_path
    )));

    for device in devices {
        if sysname == device.sysname() {
            if device.subsystem() == "block" {
                return Ok(true);
            }
        }
    }

    return Err(format!("Unable to find device with name {:?}", device_path));
}

#[derive(Clone, Debug)]
pub struct ScsiInfo {
    host: String,
    channel: u32,
    id: u32,
    lun: u32,
    vendor: RaidType,
    model: String,
    rev: String,
    scsi_type: String,
    scsi_revision: u32,
}

#[test]
fn test_scsi_parser() {
    let mut f = fs::File::open("tests/proc_scsi").unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s);
    println!("scsi_host_info {:?}", scsi_host_info(s.as_bytes()));
}
named!(host<&str>,
    map_res!(
        preceded!(ws!(tag!("Host:")), take_until_and_consume!(" ")),
        from_utf8)
);

named!(vendor<RaidType>,
    ws!(map_res!(map_res!(
        preceded!(ws!(tag!("Vendor:")), take_until_and_consume!(" ")),
        from_utf8), RaidType::from_str)
    )
);

named!(model<&str>,
    ws!(map_res!(
        preceded!(ws!(tag!("Model:")), take_until_and_consume!("  ")),
        from_utf8)
    )
);

named!(rev<&str>,
    ws!(map_res!(
        preceded!(ws!(tag!("Rev:")), take_until_and_consume!(" ")),
        from_utf8)
    )
);

named!(scsi_type<&str>,
    ws!(map_res!(
        preceded!(ws!(tag!("Type:")), take_until_and_consume!(" ")),
        from_utf8)
    )
);

named!(take_u32 <u32>,
    map_res!(
        map_res!(
            take_while!(is_digit), from_utf8),
    u32::from_str)
);

named!(channel<u32>,
    ws!(preceded!(ws!(tag!("Channel:")), take_u32))
);

named!(scsi_id<u32>,
    ws!(preceded!(ws!(tag!("Id:")), take_u32))
);

named!(scsi_lun<u32>,
    ws!(preceded!(ws!(tag!("Lun:")), take_u32))
);

named!(revision<u32>,
    ws!(preceded!(ws!(tag!("ANSI  SCSI revision:")), take_u32))
);

named!(scsi_host_info<&[u8],Vec<ScsiInfo>>,
  many1!(do_parse!(    // the parser takes a byte array as input, and returns a Vec<ScsiInfo>
    opt!(tag!("Attached devices:")) >>
    host: host >>
    channel: channel >>
    id: scsi_id >>
    lun: scsi_lun >>
    scsi_vendor: vendor >>
    scsi_model: model >>
    scsi_rev: rev >>
    scsi_type: scsi_type >>
    revision: revision >>

    // the final tuple will be able to use the variables defined previously
    (ScsiInfo{
        host: host.to_string(),
        channel: channel,
        id: id,
        lun: lun,
        vendor: scsi_vendor,
        model: scsi_model.to_string(),
        rev: scsi_rev.trim().to_string(),
        scsi_type: scsi_type.to_string(),
        scsi_revision: revision,
    })
 ))
);

/// Detects the RAID card in use
pub fn get_raid_info() -> Result<Vec<ScsiInfo>, String> {
    let mut f = fs::File::open("/proc/scsi/scsi").map_err(|e| e.to_string())?;
    let mut buff = String::new();
    f.read_to_string(&mut buff).map_err(|e| e.to_string())?;

    let parsed_data = match scsi_host_info(buff.as_bytes()) {
        nom::IResult::Done(_, o) => {
            return Ok(o);
        }
        nom::IResult::Incomplete(needed) => {
            return Err(format!(
                "Unable to parse /proc/scsi/scsi output: {}.  Needed {:?} more bytes",
                buff,
                needed));
        }
        nom::IResult::Error(e) => {
            return Err(format!(
                "Unable to parse /proc/scsi/scsi output: {}.  Error {}",
                buff,
                e));
        }
    };
}

/// Returns device info on every device it can find in the devices slice
/// The device info may not be in the same order as the slice so be aware.
/// This function is more efficient because it only call udev list once
pub fn get_all_device_info(devices: &[PathBuf]) -> Result<Vec<Device>, String> {
    let device_names: Vec<&OsStr> = devices
        .into_iter()
        .map(|d| d.file_name())
        .filter(|d| d.is_some())
        .map(|d| d.unwrap())
        .collect();
    let mut device_infos: Vec<Device> = Vec::new();

    let context = try!(libudev::Context::new().map_err(|e| e.to_string()));
    let mut enumerator = try!(libudev::Enumerator::new(&context).map_err(
        |e| e.to_string(),
    ));
    let host_devices = try!(enumerator.scan_devices().map_err(|e| e.to_string()));

    for device in host_devices {
        //let sysname = PathBuf::from(device.sysname());
        //println!("devices.contains({:?})", &sysname);
        if device_names.contains(&device.sysname()) {
            if device.subsystem() == "block" {
                // Ok we're a block device
                let id: Option<Uuid> = get_uuid(&device);
                let serial = get_serial(&device);
                let media_type = get_media_type(&device);
                let capacity = match get_size(&device) {
                    Some(size) => size,
                    None => 0,
                };
                let fs_type = get_fs_type(&device);

                device_infos.push(Device {
                    id: id,
                    name: device.sysname().to_string_lossy().into_owned(),
                    media_type: media_type,
                    capacity: capacity,
                    fs_type: fs_type,
                    serial_number: serial,
                });
            }
        }
    }
    return Ok(device_infos);
}

/// Returns device information that is gathered with udev.
pub fn get_device_info(device_path: &Path) -> Result<Device, String> {
    let context = try!(libudev::Context::new().map_err(|e| e.to_string()));
    let mut enumerator = try!(libudev::Enumerator::new(&context).map_err(
        |e| e.to_string(),
    ));
    let devices = try!(enumerator.scan_devices().map_err(|e| e.to_string()));

    let sysname = try!(device_path.file_name().ok_or(format!(
        "Unable to get file_name on device {:?}",
        device_path
    )));

    for device in devices {
        if sysname == device.sysname() {
            // This is going to get complicated
            if device.subsystem() == "block" {
                // Ok we're a block device
                let id: Option<Uuid> = get_uuid(&device);
                let serial = get_serial(&device);
                let media_type = get_media_type(&device);
                let capacity = match get_size(&device) {
                    Some(size) => size,
                    None => 0,
                };
                let fs_type = get_fs_type(&device);

                return Ok(Device {
                    id: id,
                    name: sysname.to_string_lossy().to_string(),
                    media_type: media_type,
                    capacity: capacity,
                    fs_type: fs_type,
                    serial_number: serial,
                });
            }
        }
    }
    return Err(format!("Unable to find device with name {:?}", device_path));
}
pub fn set_elevator(
    device_path: &PathBuf,
    elevator: &Scheduler,
) -> Result<usize, ::std::io::Error> {
    let device_name = match device_path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => "".to_string(),
    };
    let mut f = fs::File::open("/etc/rc.local")?;
    let elevator_cmd = format!(
        "echo {scheduler} > /sys/block/{device}/queue/scheduler",
        scheduler = elevator,
        device = device_name
    );

    let mut script = shellscript::parse(&mut f)?;
    let existing_cmd = script.commands.iter().position(
        |cmd| cmd.contains(&device_name),
    );
    if let Some(pos) = existing_cmd {
        script.commands.remove(pos);
    }
    script.commands.push(elevator_cmd);
    let mut f = fs::File::create("/etc/rc.local")?;
    let bytes_written = script.write(&mut f)?;
    Ok(bytes_written)
}

pub fn weekly_defrag(
    mount: &str,
    fs_type: &FilesystemType,
    interval: &str,
) -> Result<usize, ::std::io::Error> {
    let crontab = Path::new("/var/spool/cron/crontabs/root");
    let defrag_command = match fs_type {
        &FilesystemType::Ext4 => "e4defrag",
        &FilesystemType::Btrfs => "btrfs filesystem defragment -r",
        &FilesystemType::Xfs => "xfs_fsr",
        _ => "",
    };
    let job = format!(
        "{interval} {cmd} {path}",
        interval = interval,
        cmd = defrag_command,
        path = mount
    );

    //TODO Change over to using the cronparse library.  Has much better parsing however
    //there's currently no way to add new entries yet
    let mut existing_crontab = {
        if crontab.exists() {
            let mut buff = String::new();
            let mut f = fs::File::open("/var/spool/cron/crontabs/root")?;
            f.read_to_string(&mut buff)?;
            buff.split("\n")
                .map(|s| s.to_string())
                .collect::<Vec<String>>()
        } else {
            Vec::new()
        }
    };
    let existing_job_position = existing_crontab.iter().position(
        |line| line.contains(mount),
    );
    // If we found an existing job we remove the old and insert the new job
    if let Some(pos) = existing_job_position {
        existing_crontab.remove(pos);
    }
    existing_crontab.push(job.clone());

    //Write back out
    let mut f = fs::File::create("/var/spool/cron/crontabs/root")?;
    let written_bytes = f.write(&existing_crontab.join("\n").as_bytes())?;
    Ok(written_bytes)
}
