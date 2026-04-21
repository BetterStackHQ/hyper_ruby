# frozen_string_literal: true

require "test_helper"
require "httpx" # with_configured_server builds an HTTPX client (unused here)
require "socket"
require "timeout"

# Regression test for a silent-truncation bug: when an HTTP/2 peer sends
# HEADERS + a partial DATA frame + RST_STREAM (the frame sequence a browser
# produces when it cancels an in-flight request on page navigation), the
# server currently hands the truncated body to the Ruby handler as if the
# request had completed normally. Downstream consumers (Kafka, etc.) then
# see a short body alongside the original Content-Length header, producing
# confusingly "truncated" payloads.
#
# Correct behaviour: a stream that ends via RST_STREAM (not END_STREAM) is
# not a completed request; the handler should not run.
class TestH2StreamReset < HyperRubyTest
  PORT = 3010
  PARTIAL_BYTES = 16_384 # default HTTP/2 SETTINGS_MAX_FRAME_SIZE
  CLAIMED_CONTENT_LENGTH = 450_403

  def test_rst_stream_after_partial_data_does_not_invoke_handler
    invocations = []
    mutex = Mutex.new

    handler = lambda do |request|
      mutex.synchronize do
        invocations << {
          path: request.path,
          body_size: request.body_size,
          content_length: request.header("content-length"),
        }
      end
      HyperRuby::Response.new(200, { "Content-Type" => "text/plain" }, "ok")
    end

    config = { bind_address: "127.0.0.1:#{PORT}", tokio_threads: 1, recv_timeout: 1_000 }

    with_configured_server(config, handler) do
      send_h2_headers_data_rst(
        host: "127.0.0.1",
        port: PORT,
        partial_bytes: PARTIAL_BYTES,
        claimed_content_length: CLAIMED_CONTENT_LENGTH,
      )

      # Give the server time to hand off to the worker if it's going to.
      sleep 0.2
    end

    mutex.synchronize do
      assert_empty invocations,
        "handler should not have been invoked for a RST_STREAM'd request, but it ran with: #{invocations.inspect}"
    end
  end

  private

  H2_PREFACE       = "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".b
  TYPE_DATA        = 0x0
  TYPE_HEADERS     = 0x1
  TYPE_RST_STREAM  = 0x3
  TYPE_SETTINGS    = 0x4
  FLAG_END_HEADERS = 0x4
  FLAG_ACK         = 0x1
  RST_CODE_CANCEL  = 0x8 # what Chromium sends on navigation

  def h2_frame(type, flags, stream_id, payload)
    len = payload.bytesize
    [len >> 16 & 0xff, len >> 8 & 0xff, len & 0xff,
     type, flags, stream_id & 0x7fffffff].pack("CCCCCN") + payload
  end

  # Minimal HPACK: "literal header field, never indexed, new name" with
  # 7-bit string lengths. Names and values must fit in 126 bytes — fine for
  # everything we send here.
  def hpack_literal(name, value)
    raise "name too long"  if name.bytesize  > 126
    raise "value too long" if value.bytesize > 126
    [0x10, name.bytesize].pack("CC") + name.b +
      [value.bytesize].pack("C")      + value.b
  end

  def send_h2_headers_data_rst(host:, port:, partial_bytes:, claimed_content_length:)
    sock = TCPSocket.new(host, port)
    sock.write(H2_PREFACE)
    sock.write(h2_frame(TYPE_SETTINGS, 0,        0, "".b))
    sock.write(h2_frame(TYPE_SETTINGS, FLAG_ACK, 0, "".b))

    headers = "".b
    headers << hpack_literal(":method",        "POST")
    headers << hpack_literal(":scheme",        "http")
    headers << hpack_literal(":path",          "/rst-truncated")
    headers << hpack_literal(":authority",     "#{host}:#{port}")
    headers << hpack_literal("content-type",   "application/octet-stream")
    headers << hpack_literal("content-length", claimed_content_length.to_s)

    sock.write(h2_frame(TYPE_HEADERS,    FLAG_END_HEADERS, 1, headers))
    sock.write(h2_frame(TYPE_DATA,       0,                1, "X".b * partial_bytes))
    sock.write(h2_frame(TYPE_RST_STREAM, 0,                1, [RST_CODE_CANCEL].pack("N")))

    # Drain anything the server may have written before we hung up; we don't
    # care what it is.
    begin
      Timeout.timeout(0.5) { sock.read(4096) }
    rescue Timeout::Error
    end
  ensure
    sock&.close
  end
end
