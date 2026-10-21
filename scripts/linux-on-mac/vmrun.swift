import Foundation
import Virtualization

// vmrun KERNEL INITRD STORE JOB MEM_MIB CPUS [rosetta]
let a = CommandLine.arguments
let cfg = VZVirtualMachineConfiguration()
let boot = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: a[1]))
boot.initialRamdiskURL = URL(fileURLWithPath: a[2])
boot.commandLine = "console=hvc0 rdinit=/init quiet"
cfg.bootLoader = boot
cfg.memorySize = UInt64(a[5])! << 20
cfg.cpuCount = Int(a[6])!
let con = VZVirtioConsoleDeviceSerialPortConfiguration()
con.attachment = VZFileHandleSerialPortAttachment(fileHandleForReading: nil, fileHandleForWriting: FileHandle.standardError)
cfg.serialPorts = [con]
func share(_ tag: String, _ path: String, ro: Bool) -> VZVirtioFileSystemDeviceConfiguration {
  let d = VZVirtioFileSystemDeviceConfiguration(tag: tag)
  d.share = VZSingleDirectoryShare(directory: VZSharedDirectory(url: URL(fileURLWithPath: path), readOnly: ro))
  return d
}
var shares = [share("store", a[3], ro: true), share("job", a[4], ro: false)]
if a.count > 7 && a[7] == "rosetta" {
  fputs("rosetta: \(VZLinuxRosettaDirectoryShare.availability.rawValue)\n", stderr)
  let r = VZVirtioFileSystemDeviceConfiguration(tag: "rosetta")
  r.share = try VZLinuxRosettaDirectoryShare()
  shares.append(r)
}
cfg.directorySharingDevices = shares
cfg.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
try cfg.validate()
let vm = VZVirtualMachine(configuration: cfg)
final class Stop: NSObject, VZVirtualMachineDelegate {
  func guestDidStop(_ vm: VZVirtualMachine) { exit(0) }
  func virtualMachine(_ vm: VZVirtualMachine, didStopWithError e: Error) { fputs("vm: \(e)\n", stderr); exit(1) }
}
let stop = Stop()
vm.delegate = stop
vm.start { r in if case .failure(let e) = r { fputs("start: \(e)\n", stderr); exit(1) } }
RunLoop.main.run()
