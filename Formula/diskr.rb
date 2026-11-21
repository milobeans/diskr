class Diskr < Formula
  desc "Explore disk usage, packages, and cleanup candidates in the macOS terminal"
  homepage "https://milobeans.github.io/diskr/"
  version "0.1.84"
  license "MIT"

  on_arm do
    url "https://github.com/milobeans/diskr/releases/download/v0.1.84/diskr-aarch64-apple-darwin.tar.gz"
    sha256 "3b1b6dcca6bdf5c8d667c1e4b3eba5c25b5a2bbd7dd8d453db8746644319f1d8"
  end

  on_intel do
    url "https://github.com/milobeans/diskr/releases/download/v0.1.84/diskr-x86_64-apple-darwin.tar.gz"
    sha256 "72bc22fee9b5554a7a0137146eb0ecb9f964501749404e0a0a7332777ed87f23"
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
