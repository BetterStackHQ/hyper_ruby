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
    block.call(client)

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
