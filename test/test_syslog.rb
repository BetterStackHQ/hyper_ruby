# frozen_string_literal: true

require "test_helper"
require "socket"
require "ipaddr"
require "net/http"
require "concurrent"

class TestSyslog < HyperRubyTest
  PROXY_V2_SIGNATURE = "\x0d\x0a\x0d\x0a\x00\x0d\x0a\x51\x55\x49\x54\x0a".b

  def setup
    @collector = Collector.new
  end

  # Collects delivered messages and decides what the handler returns.
  class Collector
    attr_reader :messages
    attr_writer :delay, :accept_from_attempt, :refuse_matching

    def initialize
      @mutex = Mutex.new
      @messages = []
      @accept = true
      @delay = 0
      @accept_from_attempt = 1
      @refuse_matching = nil
    end

    def handler
      lambda do |message, peer, transport, received_at_ns, message_id, attempt|
        sleep(@delay) if @delay > 0
        @mutex.synchronize do
          @messages << {
            message: message, peer: peer, transport: transport,
            received_at_ns: received_at_ns, message_id: message_id, attempt: attempt
          }
        end

        next false if @refuse_matching && message.include?(@refuse_matching)
        next false if attempt < @accept_from_attempt

        @accept
      end
    end

    def accept!
      @accept = true
    end

    def refuse!
      @accept = false
    end

    def bodies
      @mutex.synchronize { @messages.map { |m| m[:message] } }
    end

    def count
      @mutex.synchronize { @messages.size }
    end

    def for_message(body)
      @mutex.synchronize { @messages.select { |m| m[:message] == body } }
    end
  end

  def test_octet_counted_and_newline_frames
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      before = Time.now.to_f * 1_000_000_000
      # The handler must survive collection between configuration and delivery.
      GC.start(full_mark: true, immediate_sweep: true)

      connect_stream(server) do |socket|
        socket.write("11 hello there")
        socket.write("<13>a newline frame\n")
        socket.write("<13>a crlf frame\r\n")
        wait_until { @collector.count == 3 }
      end

      assert_equal ["hello there", "<13>a newline frame", "<13>a crlf frame"], @collector.bodies

      first = @collector.messages.first
      assert_equal "127.0.0.1", first[:peer]
      assert_equal :stream, first[:transport]
      assert_equal Encoding::UTF_8, first[:message].encoding
      assert_operator first[:received_at_ns], :>=, before
      assert_equal 1, first[:attempt]
      assert_equal 3, @collector.messages.map { |m| m[:message_id] }.uniq.size
      assert_equal 3, server.syslog_stats[:messages_delivered]
    end
  end

  def test_frames_split_across_writes
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      payload = "24 <13>an octet counted one<13>and a line\n"
      connect_stream(server) do |socket|
        payload.each_char do |char|
          socket.write(char)
          socket.flush
        end
        wait_until { @collector.count == 2 }
      end

      assert_equal ["<13>an octet counted one", "<13>and a line"], @collector.bodies
    end
  end

  def test_newline_oversize_frame_is_skipped_but_connection_survives
    config = syslog_config(stream: true).merge(syslog_max_frame_bytes: 32)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("<13>#{'a' * 64}\n")
        socket.write("<13>short enough\n")
        wait_until { @collector.count == 1 }

        assert_equal ["<13>short enough"], @collector.bodies
        assert_equal 1, server.syslog_stats[:frames_rejected][:oversize]
      end
    end
  end

  def test_octet_counted_oversize_frame_closes_connection
    config = syslog_config(stream: true).merge(syslog_max_frame_bytes: 32)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("64 #{'a' * 64}")
        # The declared bytes are discarded as they arrive, and the frame is
        # rejected once the last of them is in.
        sleep 0.05
        socket.write("<13>never read\n")
        assert_closed(socket)
      end

      assert_equal [], @collector.bodies
      assert_equal 1, server.syslog_stats[:frames_rejected][:oversize]
    end
  end

  def test_oversize_body_starting_with_a_digit_is_not_delivered
    config = syslog_config(stream: true).merge(syslog_max_frame_bytes: 16)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("26 ")
        sleep 0.05
        socket.write("9 abcdefghZ")
        sleep 0.05
        socket.write("aaaaaaaaaaaaaaa")
        assert_closed(socket)
      end

      assert_equal [], @collector.bodies
      assert_equal 1, server.syslog_stats[:frames_rejected][:oversize]
    end
  end

  def test_invalid_utf8_closes_connection
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("11 valid frame")
        socket.write("4 \xf0\x28\x8c\xbc".b)
        socket.write("<13>never read\n")
        assert_closed(socket)
      end

      assert_equal ["valid frame"], @collector.bodies
      assert_equal 1, server.syslog_stats[:frames_rejected][:invalid_utf8]
    end
  end

  def test_unix_socket_stream_listener
    path = "/tmp/hyper_ruby_syslog_test_#{Process.pid}.sock"
    config = syslog_config.merge(syslog_stream_path: path)

    with_syslog_server(config, @collector.handler) do |server|
      assert server.syslog_listening?

      socket = UNIXSocket.new(path)
      begin
        socket.write("<13>over a unix socket\n")
        wait_until { @collector.count == 1 }
      ensure
        socket.close
      end

      assert_equal ["<13>over a unix socket"], @collector.bodies
      assert_nil @collector.messages.first[:peer]
    end

    refute File.exist?(path), "the socket file should be removed on stop"
  end

  def test_udp_datagram_with_embedded_newlines_is_one_message
    with_syslog_server(syslog_config(udp: true), @collector.handler) do |server|
      send_datagram(server, "<13>first line\n<13>second line\n")
      wait_until { @collector.count == 1 }

      message = @collector.messages.first
      assert_equal "<13>first line\n<13>second line\n", message[:message]
      assert_equal Encoding::ASCII_8BIT, message[:message].encoding
      assert_equal :datagram, message[:transport]
      assert_equal "127.0.0.1", message[:peer]
      assert_equal 1, server.syslog_stats[:messages_delivered]
    end
  end

  def test_udp_datagram_keeps_invalid_utf8_bytes
    with_syslog_server(syslog_config(udp: true), @collector.handler) do |server|
      send_datagram(server, "<13>\xf0\x28\x8c\xbc".b)
      wait_until { @collector.count == 1 }

      assert_equal "<13>\xf0\x28\x8c\xbc".b, @collector.messages.first[:message]
      assert_equal 1, server.syslog_stats[:messages_delivered]
    end
  end

  def test_truncated_udp_datagram_is_dropped_and_counted
    config = syslog_config(udp: true).merge(syslog_udp_max_datagram_bytes: 64)
    with_syslog_server(config, @collector.handler) do |server|
      send_datagram(server, "<13>#{'a' * 256}")
      send_datagram(server, "<13>small one")
      wait_until { @collector.count == 1 }

      assert_equal ["<13>small one"], @collector.bodies
      assert_equal 1, server.syslog_stats[:udp_truncated]
    end
  end

  def test_proxy_protocol_supplies_the_peer_address
    config = syslog_config(stream: true).merge(syslog_proxy_protocol: true)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write(proxy_v2_header("192.0.2.7"))
        socket.write("<13>behind a proxy\n")
        wait_until { @collector.count == 1 }
      end

      assert_equal ["<13>behind a proxy"], @collector.bodies
      assert_equal "192.0.2.7", @collector.messages.first[:peer]
      assert_equal 0, server.syslog_stats[:proxy_header_errors]
    end
  end

  def test_proxy_protocol_header_split_across_writes
    config = syslog_config(stream: true).merge(syslog_proxy_protocol: true)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        header = proxy_v2_header("198.51.100.4")
        header.each_char do |char|
          socket.write(char)
          socket.flush
        end
        socket.write("<13>split header\n")
        wait_until { @collector.count == 1 }
      end

      assert_equal "198.51.100.4", @collector.messages.first[:peer]
    end
  end

  def test_missing_proxy_header_closes_connection
    config = syslog_config(stream: true).merge(syslog_proxy_protocol: true)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("<13>no proxy header\n")
        assert_closed(socket)
      end

      assert_equal [], @collector.bodies
      wait_until { server.syslog_stats[:proxy_header_errors] == 1 }
      assert_equal 1, server.syslog_stats[:proxy_header_errors]
      assert_equal 0, server.syslog_stats[:proxy_read_errors]
    end
  end

  def test_garbled_proxy_header_closes_connection
    config = syslog_config(stream: true).merge(syslog_proxy_protocol: true)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        header = proxy_v2_header("192.0.2.7").dup
        header[5] = "\xff".b
        socket.write(header)
        socket.write("<13>never read\n")
        assert_closed(socket)
      end

      assert_equal [], @collector.bodies
      wait_until { server.syslog_stats[:proxy_header_errors] == 1 }
      assert_equal 1, server.syslog_stats[:proxy_header_errors]
    end
  end

  def test_connection_closed_before_the_proxy_header_counts_as_a_read_error
    config = syslog_config(stream: true).merge(syslog_proxy_protocol: true)
    with_syslog_server(config, @collector.handler) do |server|
      socket = TCPSocket.new("127.0.0.1", @stream_port)
      socket.write(PROXY_V2_SIGNATURE[0, 6])
      socket.close

      wait_until { server.syslog_stats[:proxy_read_errors] == 1 }
      assert_equal 1, server.syslog_stats[:proxy_read_errors]
      assert_equal 0, server.syslog_stats[:proxy_header_errors]
    end
  end

  def test_refused_messages_stop_reads_and_are_retried
    config = syslog_config(stream: true).merge(syslog_max_pending_per_connection: 1)
    with_syslog_server(config, @collector.handler) do |server|
      @collector.refuse!

      connect_stream(server) do |socket|
        20.times { |i| socket.write("<13>message #{i}\n") }
        socket.flush

        # The refused message is retried, and nothing behind it is delivered.
        wait_until { @collector.count > 3 }
        assert_equal ["<13>message 0"], @collector.bodies.uniq
        assert_operator server.syslog_stats[:deliveries_refused], :>=, 3

        @collector.accept!
        wait_until { @collector.bodies.uniq.size == 20 }
        assert_equal 20, @collector.bodies.uniq.size
      end
    end
  end

  def test_retries_repeat_the_message_id_and_count_attempts
    @collector.accept_from_attempt = 3

    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("<13>retried\n")
        socket.write("<13>plain\n")
        wait_until { @collector.bodies.include?("<13>plain") }
      end

      retried = @collector.for_message("<13>retried")
      assert_equal [1, 2, 3], retried.map { |m| m[:attempt] }
      assert_equal 1, retried.map { |m| m[:message_id] }.uniq.size

      plain = @collector.for_message("<13>plain")
      assert_equal [1, 2, 3], plain.map { |m| m[:attempt] }
      assert_equal 1, plain.map { |m| m[:message_id] }.uniq.size
      refute_equal retried.first[:message_id], plain.first[:message_id]
      assert_equal 2, server.syslog_stats[:messages_delivered]
      assert_equal 4, server.syslog_stats[:deliveries_refused]
    end
  end

  def test_one_stalled_connection_does_not_block_another
    config = syslog_config(stream: true).merge(
      syslog_max_pending: 100,
      syslog_max_pending_per_connection: 2
    )

    with_syslog_server(config, @collector.handler) do |server|
      @collector.refuse_matching = "stalled"

      connect_stream(server) do |stalled|
        20.times { |i| stalled.write("<13>stalled #{i}\n") }
        stalled.flush
        wait_until { @collector.count >= 2 }

        healthy = TCPSocket.new("127.0.0.1", @stream_port)
        begin
          5.times { |i| healthy.write("<13>healthy #{i}\n") }
          healthy.flush
          wait_until { @collector.bodies.count { |body| body.include?("healthy") } == 5 }
        ensure
          healthy.close
        end

        assert_equal 5, @collector.bodies.uniq.count { |body| body.include?("healthy") }
        assert_operator @collector.bodies.uniq.count { |body| body.include?("stalled") }, :<=, 3
        assert_equal 5, server.syslog_stats[:messages_delivered]
      end
    end
  end

  def test_refused_udp_datagrams_are_dropped_and_counted
    with_syslog_server(syslog_config(udp: true), @collector.handler) do |server|
      @collector.refuse!
      send_datagram(server, "<13>refused datagram")
      wait_until { server.syslog_stats[:udp_dropped] == 1 }

      assert_equal 1, server.syslog_stats[:udp_dropped]
      assert_equal 0, server.syslog_stats[:messages_delivered]
      assert_equal 1, @collector.count
    end
  end

  def test_handler_exception_refuses_the_message_and_counts_separately
    raised = Queue.new
    handler = lambda do |*_args|
      raised << true
      raise "handler failure"
    end

    with_syslog_server(syslog_config(udp: true), handler) do |server|
      send_datagram(server, "<13>boom")
      wait_until { server.syslog_stats[:udp_dropped] == 1 }

      stats = server.syslog_stats
      assert_equal 1, stats[:handler_errors]
      assert_equal 0, stats[:deliveries_refused]
      assert_equal 0, stats[:messages_delivered]
      refute raised.empty?
    end
  end

  def test_shutdown_delivers_already_framed_messages
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      @collector.delay = 0.02

      socket = TCPSocket.new("127.0.0.1", @stream_port)
      begin
        10.times { |i| socket.write("<13>message #{i}\n") }
        socket.flush

        # Let the listener read and frame everything before it is told to stop.
        wait_until { @collector.count >= 1 }
        sleep 0.1

        server.stop
        assert_equal 10, @collector.count
        assert_equal 10, server.syslog_stats[:messages_delivered]
        assert_equal 0, server.syslog_stats[:abandoned_at_shutdown]
        refute server.syslog_listening?
      ensure
        socket.close
      end
    end
  end

  def test_shutdown_counts_messages_it_could_not_admit
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      @collector.refuse!

      socket = TCPSocket.new("127.0.0.1", @stream_port)
      begin
        socket.write("<13>never admitted\n")
        socket.flush
        wait_until { server.syslog_stats[:deliveries_refused] >= 1 }

        server.stop
        stats = server.syslog_stats
        assert_equal 1, stats[:abandoned_at_shutdown]
        assert_equal 0, stats[:messages_delivered]
        assert_equal 0, stats[:pending]
      ensure
        socket.close
      end
    end
  end

  def test_connections_beyond_the_limit_are_refused
    config = syslog_config(stream: true).merge(syslog_max_connections: 1)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("<13>the only connection\n")
        wait_until { @collector.count == 1 }

        refused = TCPSocket.new("127.0.0.1", @stream_port)
        begin
          assert_closed(refused)
        ensure
          refused.close
        end

        assert_equal 1, server.syslog_stats[:connections_refused]
        socket.write("<13>still connected\n")
        wait_until { @collector.count == 2 }
        assert_equal 2, @collector.count
      end
    end
  end

  def test_idle_connections_are_closed
    config = syslog_config(stream: true).merge(syslog_idle_timeout_ms: 200)
    with_syslog_server(config, @collector.handler) do |server|
      connect_stream(server) do |socket|
        socket.write("<13>then silence\n")
        wait_until { @collector.count == 1 }
        assert_closed(socket)
      end

      wait_until { server.syslog_stats[:connections_closed] == 1 }
      assert_equal 1, server.syslog_stats[:connections_closed]
    end
  end

  def test_listening_and_connection_counters
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      assert server.syslog_listening?

      connect_stream(server) do |socket|
        socket.write("<13>counted\n")
        wait_until { @collector.count == 1 }
      end
      wait_until { server.syslog_stats[:connections_closed] == 1 }

      stats = server.syslog_stats
      assert_equal 1, stats[:connections_opened]
      assert_equal 1, stats[:connections_closed]
      assert_equal 0, stats[:connections_refused]
      assert_equal({ oversize: 0, invalid_utf8: 0, invalid_length: 0 }, stats[:frames_rejected])
    end
  end

  def test_http_still_works_alongside_syslog
    with_syslog_server(syslog_config(stream: true), @collector.handler) do |server|
      response = Net::HTTP.get_response(URI("http://127.0.0.1:#{@http_port}/"))
      assert_equal "200", response.code

      connect_stream(server) do |socket|
        socket.write("<13>after an http request\n")
        wait_until { @collector.count == 1 }
      end

      assert_equal ["<13>after an http request"], @collector.bodies
    end
  end

  def test_syslog_delivery_outpaces_requests_under_http_load
    # A handler slow enough that the request queue never empties, which is what
    # would let requests starve syslog messages.
    handled = Concurrent::AtomicFixnum.new(0)
    slow_request = lambda do |_request|
      sleep 0.005
      handled.increment
      HyperRuby::Response.new(200, {}, "")
    end

    with_syslog_server(syslog_config(stream: true), @collector.handler,
                       request_handler: slow_request) do |server|
      flooding = true
      floods = 8.times.map do
        Thread.new do
          Net::HTTP.start("127.0.0.1", @http_port) do |http|
            http.request(Net::HTTP::Get.new("/")) while flooding
          end
        rescue IOError, EOFError, SystemCallError
          nil
        end
      end

      begin
        wait_until { handled.value > 5 }
        before = handled.value

        sockets = 5.times.map { TCPSocket.new("127.0.0.1", @stream_port) }
        begin
          sockets.each_with_index do |socket, connection|
            10.times { |i| socket.write("<13>under load #{connection}-#{i}\n") }
            socket.flush
          end
          wait_until(timeout: 10) { @collector.count == 50 }
        ensure
          sockets.each(&:close)
        end

        during = handled.value - before
        assert_equal 50, @collector.count
        assert_equal 50, server.syslog_stats[:messages_delivered]
        assert_operator during, :<, 25,
                        "syslog should outpace requests at the configured work ratio, saw #{during} requests"
      ensure
        flooding = false
        floods.each(&:join)
      end
    end
  end

  private

  @@next_port = 3400

  def next_port
    @@next_port += 1
  end

  def syslog_config(stream: false, udp: false)
    @http_port = next_port
    config = { bind_address: "127.0.0.1:#{@http_port}", tokio_threads: 1 }

    if stream
      @stream_port = next_port
      config[:syslog_stream_bind] = "127.0.0.1:#{@stream_port}"
    end

    if udp
      @udp_port = next_port
      config[:syslog_udp_bind] = "127.0.0.1:#{@udp_port}"
    end

    config
  end

  def connect_stream(_server)
    socket = TCPSocket.new("127.0.0.1", @stream_port)
    yield socket
  ensure
    socket.close if socket && !socket.closed?
  end

  def send_datagram(_server, payload, port: nil)
    socket = UDPSocket.new
    socket.send(payload, 0, "127.0.0.1", port || @udp_port)
  ensure
    socket.close if socket
  end

  def proxy_v2_header(source_ip)
    address = IPAddr.new(source_ip).hton + IPAddr.new("10.0.0.1").hton + [514, 6514].pack("nn")
    PROXY_V2_SIGNATURE + [0x21, 0x11, address.bytesize].pack("CCn") + address
  end

  def assert_closed(socket, timeout: 3)
    deadline = Time.now + timeout
    loop do
      remaining = deadline - Time.now
      flunk "connection was not closed by the server" if remaining <= 0

      begin
        return if socket.read_nonblock(1024, exception: false) == nil
      rescue EOFError, Errno::ECONNRESET, IOError
        return
      end

      IO.select([socket], nil, nil, 0.05)
    end
  end

  def wait_until(timeout: 5)
    deadline = Time.now + timeout
    sleep(0.01) until yield || Time.now > deadline
    yield
  end
end
