# frozen_string_literal: true

$LOAD_PATH.unshift File.expand_path("../lib", __dir__)
require "hyper_ruby"
require "minitest/autorun"

class HyperRubyTest < Minitest::Test
  def with_server(request_handler, &block)
    with_configured_server({ 
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      #debug: true 
    }, request_handler, &block)
  end

  def with_configured_server(config, request_handler, &block)
    server = HyperRuby::Server.new
    server.configure(config)
    server.start
    
    # Create ruby worker threads that process requests;
    # 1 is usually enough, and generally handles better than multiple threads 
    # if there's no IO (because of the GIL)
    workers = 1.times.map do
      Thread.new do
        server.run_worker do |request|
          # Process the request in Ruby
          request_handler.call(request)
        end
      end
    end

    client = HTTPX.with(origin: "http://127.0.0.1:3010")
    block.call(client, server)

  ensure
    server.stop if server
    workers.map(&:join) if workers
  end

  # Starts a server with syslog listeners configured; the syslog handler is
  # called with (message, peer_ip, transport, received_at_ns) on a worker thread.
  def with_syslog_server(config, syslog_handler, worker_count: 1, &block)
    server = HyperRuby::Server.new
    server.configure(config.merge(syslog_handler: syslog_handler))
    server.start

    workers = worker_count.times.map do
      Thread.new do
        server.run_worker do |_request|
          HyperRuby::Response.new(200, {}, "")
        end
      end
    end

    block.call(server)

  ensure
    server.stop if server
    workers.map(&:join) if workers
  end

  def with_unix_socket_server(request_handler, &block)
    server = HyperRuby::Server.new
    server.configure({ 
      bind_address: "unix:/tmp/hyper_ruby_test.sock", 
      tokio_threads: 1,
      #debug: true 
    })
    server.start
    
    workers = 2.times.map do
      Thread.new do
        server.run_worker do |request|
          request_handler.call(request)
        end
      end
    end

    client = HTTPX.with(transport: "unix", addresses: ["/tmp/hyper_ruby_test.sock"], origin: "http://host")
    block.call(client)

  ensure
    server.stop if server
    workers.map(&:join) if workers
  end
end
