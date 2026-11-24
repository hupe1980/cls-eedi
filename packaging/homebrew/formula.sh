#!/usr/bin/env bash
# Render the stable tap formula from the version and checksum of the published crate.
set -euo pipefail
version=${1:?usage: formula.sh VERSION SHA256}
checksum=${2:?usage: formula.sh VERSION SHA256}
if ! [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ && $checksum =~ ^[0-9a-f]{64}$ ]]; then
  echo "Expected a stable Cargo version and a lowercase SHA-256 digest." >&2
  exit 1
fi

cat <<RUBY
class ClsEedi < Formula
  desc "Inspect CLS.EEDI grid controls and device state"
  homepage "https://crates.io/crates/cls-eedi"
  url "https://static.crates.io/crates/cls-eedi/cls-eedi-${version}.crate"
  version "${version}"
  sha256 "${checksum}"
  license any_of: ["MIT", "Apache-2.0"]

  depends_on "rust" => :build

  def install
    system "cargo", "install", "--no-track", "--locked", "--path", ".", "--root", prefix, "--bin", "cls-eedi"
  end

  test do
    assert_equal "cls-eedi #{version}", shell_output("#{bin}/cls-eedi --version").strip
    message = '{"type":"de.keo-connectivity.clseedi.read","id":"probe","source":"brew","specversion":"1.0","data":{"protocol":"1.2.0","parameters":["limits"]}}'
    assert_match "valid read (CLS.EEDI 1.2.0)", pipe_output("#{bin}/cls-eedi check", message, 0)
  end
end
RUBY
