use failure::{Error, Fail, ResultExt};
use serde_json;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use target_info::Target;

/// This snippets will give us information about the python interpreter's version and abi
/// as json through stdout
const GET_INTERPRETER_METADATA: &str = r##"
import sysconfig
import sys
import json

print(json.dumps({
    "major": sys.version_info.major,
    "minor": sys.version_info.minor,
    "abiflags": sysconfig.get_config_var("ABIFLAGS"),
    "m": sysconfig.get_config_var("WITH_PYMALLOC") == 1,
    "u": sysconfig.get_config_var("Py_UNICODE_SIZE") == 4,
    "d": sysconfig.get_config_var("Py_DEBUG") == 1,
    # This one isn't technically necessary, but still very useful for sanity checks
    "platform": sys.platform,
}))
"##;

/// The output format of [GET_INTERPRETER_METADATA]
#[derive(Serialize, Deserialize)]
struct IntepreterMetadataMessage {
    major: usize,
    minor: usize,
    abiflags: Option<String>,
    m: bool,
    u: bool,
    d: bool,
    platform: String,
}

/// The location and version of an interpreter
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct PythonInterpreter {
    /// Python's major version
    pub major: usize,
    /// Python's minor version
    pub minor: usize,
    /// For linux and mac, this contains the value of the abiflags, e.g. "m" for python3.5m or
    /// "mu" for python2.7mu. On windows, the value is always "".
    ///
    /// See PEP 261 and PEP 393 for details
    pub abiflags: String,
    /// Currently just the value of [Target::os()], i.e. "windows", "linux" or "macos"
    pub target: String,
    /// The value of `sys.platform`. One of "win32"
    /// Path to the python interpreter, e.g. /usr/bin/python3.6
    ///
    /// Just the name of the binary in PATH does also work, e.g. `python3.5`
    pub executable: PathBuf,
}

/// Returns the abiflags that are assembled through the message, with some additional sanity
/// checks.
///
/// The rules are as follows:
///  - python 2 + Unix: Assemble the individual parts (m/u/d), no ABIFLAGS
///  - python 2 + Windows: no ABIFLAGS, parts, return an empty string
///  - python 3 + Unix: Use ABIFLAGS
///  - python 3 + Windows: No ABIFLAGS, return an empty string
fn fun_with_abiflags(message: &IntepreterMetadataMessage) -> Result<String, Error> {
    if message.major == 2 {
        let mut abiflags = String::new();
        if message.m {
            abiflags += "m";
        }
        if message.u {
            abiflags += "u";
        }
        if message.d {
            abiflags += "d";
        }

        if message.abiflags.is_some() {
            bail!("A python 2 interpreter does not define abiflags in its sysconfig ಠ_ಠ")
        }

        if abiflags != "" && Target::os() == "windows" {
            bail!("A python 2 interpreter on windows does not define abiflags in its sysconfig ಠ_ಠ")
        }

        Ok(abiflags)
    } else if message.major == 3 && message.minor >= 5 {
        if Target::os() == "windows" {
            if message.abiflags.is_some() {
                bail!("A python 3 interpreter on windows does not define abiflags in its sysconfig ಠ_ಠ")
            } else {
                Ok("".to_string())
            }
        } else if Target::os() == "linux" || Target::os() == "macos" {
            if let Some(ref abiflags) = message.abiflags {
                if abiflags != "m" {
                    bail!("A python 3 interpreter on linux or mac os must have 'm' as abiflags ಠ_ಠ")
                }
                Ok(abiflags.clone())
            } else {
                bail!("A python 3 interpreter on linux or mac os must define abiflags in its sysconfig ಠ_ಠ")
            }
        } else {
            bail!("I'm running on a platform that is neither window, nor linux, nor mac os ಠ_ಠ")
        }
    } else {
        bail!("Only python 2.7 and python 3.x are supported");
    }
}

/// Check that sys.platform and Target::os() match
fn check_platform_sanity(message: &IntepreterMetadataMessage) -> Result<(), Error> {
    let sane_platform = match message.platform.as_ref() {
        "win32" | "win_amd64" => Target::os() == "windows",
        "linux" | "linux2" | "linux3" => Target::os() == "linux",
        "darwin" => Target::os() == "macos",
        _ => false,
    };
    if !sane_platform {
        bail!(
            "sys.platform in python, {}, and Target::os() in rust, {}, don't match ಠ_ಠ",
            message.platform,
            Target::os()
        )
    }

    Ok(())
}

impl PythonInterpreter {
    /// Returns the supported python environment in the PEP 425 format:
    /// {python tag}-{abi tag}-{platform tag}
    pub fn get_tag(&self) -> String {
        // Don't ask me why, this is just what setuptools uses so I'm also going to use it
        let platform = match self.target.as_ref() {
            "linux" => "manylinux1_x86_64",
            "macos" => {
                "macosx_10_6_intel.\
                 macosx_10_9_intel.\
                 macosx_10_9_x86_64.\
                 macosx_10_10_intel.\
                 macosx_10_10_x86_64"
            }
            "windows" => if Target::pointer_width() == "64" {
                "win_amd64"
            } else {
                "win32"
            },
            _ => panic!("This platform is not supported"),
        };
        format!(
            "cp{major}{minor}-cp{major}{minor}{abiflags}-{platform}",
            major = self.major,
            minor = self.minor,
            abiflags = self.abiflags,
            platform = platform
        )
    }

    /// Generates the correct suffix for shared libraries
    ///
    /// Note that PEP 3149 is only valid for 3.2 - 3.4 for mac and linux and the 3.5. The templates
    /// are adapted from the (also incorrect) release notes of python 3.5:
    /// https://docs.python.org/3/whatsnew/3.5.html#build-and-c-api-changes
    ///
    /// Examples for x86 on Python 3.5m:
    /// Linux:   steinlaus.cpython-35m-x86_64-linux-gnu.so
    /// Windows: steinlaus.cp35-win_amd64.pyd
    /// Mac:     steinlaus.cpython-35m-darwin.so
    pub fn get_library_extension(&self) -> String {
        if self.major == 2 {
            return ".so".to_string();
        }

        match self.target.as_ref() {
            "linux" => format!(
                ".cpython-{major}{minor}{abiflags}-{architecture}-{os}.so",
                major = self.major,
                minor = self.minor,
                abiflags = self.abiflags,
                architecture = Target::arch(),
                os = format!("{}-{}", Target::os(), Target::env()),
            ),
            "macos" => format!(
                ".cpython-{major}{minor}{abiflags}-darwin.so",
                major = self.major,
                minor = self.minor,
                abiflags = self.abiflags,
            ),
            "windows" => format!(
                ".cp{major}{minor}-{platform}.pyd",
                major = self.major,
                minor = self.minor,
                platform = if Target::pointer_width() == "64" {
                    "win_amd64"
                } else {
                    "win32"
                },
            ),
            _ => panic!("This platform is not supported"),
        }
    }

    /// Checks which python version of a set of possible versions are avaible and determins whether
    /// they are m or mu
    pub fn find_all(python_versions: &[String]) -> Result<Vec<PythonInterpreter>, Error> {
        let mut available_versions = Vec::new();
        for executable in python_versions {
            let output = Command::new(&executable)
                .args(&["-c", GET_INTERPRETER_METADATA])
                .stderr(Stdio::inherit())
                .output();

            let err_msg = format!(
                "Trying to get metadata from the python interpreter {} failed",
                executable
            );

            let output = match output {
                Ok(output) => {
                    if output.status.success() {
                        output
                    } else {
                        bail!(err_msg);
                    }
                }
                Err(err) => {
                    if err.kind() == io::ErrorKind::NotFound {
                        continue;
                    } else {
                        bail!(err.context(err_msg));
                    }
                }
            };
            let message: IntepreterMetadataMessage =
                serde_json::from_slice(&output.stdout).context(err_msg)?;

            check_platform_sanity(&message)?;

            let abiflags = fun_with_abiflags(&message)
                .context("Failed to get information from the python interpreter")?;

            available_versions.push(PythonInterpreter {
                major: message.major,
                minor: message.minor,
                abiflags,
                target: Target::os().to_string(),
                executable: PathBuf::from(executable),
            });
        }

        Ok(available_versions)
    }
}

impl fmt::Display for PythonInterpreter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Python {}.{}{} at {}",
            self.major,
            self.minor,
            self.abiflags,
            self.executable.display()
        )
    }
}