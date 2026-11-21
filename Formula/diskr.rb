class Diskr < Formula
  desc "Explore disk usage, packages, and cleanup candidates in the macOS terminal"
  homepage "https://milobeans.github.io/diskr/"
  version "0.1.85"
  license "MIT"

  on_arm do
    url "https://github.com/milobeans/diskr/releases/download/v0.1.85/diskr-aarch64-apple-darwin.tar.gz"
    sha256 "945ed93274703f312c8fd26729791af78077918885d29f72d88c2b68eb21f30c"
  end

  on_intel do
    url "https://github.com/milobeans/diskr/releases/download/v0.1.85/diskr-x86_64-apple-darwin.tar.gz"
    sha256 "39f90de0fc35d11a34a9e786284a925cb4ae0ee82abed2018633f80b0fef214c"
  end

  depends_on :macos
  depends_on macos: :sequoia

  def install
    bin.install "diskr"
    prefix.install "LICENSE"
    pkgshare.install "licenses"
  end

  test do
    assert_equal "diskr #{version}", shell_output("#{bin}/diskr --version").strip
    (testpath/"sample.txt").write "diskr test\n"
    report = JSON.parse(shell_output("#{bin}/diskr --top 1 --json #{testpath}"))
    assert_equal 1, report.fetch("schema_version")
    assert_match "sample.txt", report.to_json
  end
end
