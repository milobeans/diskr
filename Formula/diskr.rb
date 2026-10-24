class Diskr < Formula
  desc "Explore disk usage, packages, and cleanup candidates in the macOS terminal"
  homepage "https://milobeans.github.io/diskr/"
  version "0.1.83"
  license "MIT"

  on_arm do
    url "https://github.com/milobeans/diskr/releases/download/v0.1.83/diskr-aarch64-apple-darwin.tar.gz"
    sha256 "16d9579803894f098ef57ba3ed2e6a4165c89b6aa34ef91427b70db1f140d147"
  end

  on_intel do
    url "https://github.com/milobeans/diskr/releases/download/v0.1.83/diskr-x86_64-apple-darwin.tar.gz"
    sha256 "18f76a6eda7e689d7349f33fd7ff27e2186b33f77989d880911ee07907386d9b"
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
