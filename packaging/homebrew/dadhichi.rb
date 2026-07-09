# Homebrew formula for Dadhichi.
#
# Install from this repository as a tap:
#
#   brew tap pariharshyamu/dadhichi https://github.com/pariharshyamu/Dadhichi
#   brew install dadhichi                 # latest tagged release (from source)
#   brew install --HEAD dadhichi          # build the tip of the main branch
#
# The `url`/`sha256` for the stable build are filled in automatically by the
# release workflow (see packaging/homebrew/update-formula.sh); until the first
# tag is cut, use `--HEAD`.
class Dadhichi < Formula
  desc "Modern, AI-first, agent-native IDE written in Rust"
  homepage "https://github.com/pariharshyamu/Dadhichi"
  license "Apache-2.0"
  head "https://github.com/pariharshyamu/Dadhichi.git", branch: "main"

  # BEGIN stable — managed by packaging/homebrew/update-formula.sh
  url "https://github.com/pariharshyamu/Dadhichi/archive/refs/tags/v0.1.12.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  version "0.1.12"
  # END stable

  depends_on "rust" => :build

  def install
    # Build and install just the `dadhichi` binary from the workspace.
    system "cargo", "install", *std_cargo_args(path: "crates/dadhichi")

    # Ship the man page alongside the binary.
    man1.install "packaging/linux/dadhichi.1"
  end

  test do
    # The binary reports its crate version and prints usage help offline.
    assert_match "dadhichi #{version}", shell_output("#{bin}/dadhichi --version")
    assert_match "USAGE:", shell_output("#{bin}/dadhichi --help")
  end
end
