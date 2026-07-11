// GNU tar interprets a colon in an archive operand as a remote-host
// separator. Git for Windows therefore treats `C:\\...\\package.tgz` as an
// attempt to connect to host `C` unless `--force-local` is supplied.
export function tarArgs(arguments_, platform = process.platform) {
  return platform === "win32" ? ["--force-local", ...arguments_] : arguments_
}
