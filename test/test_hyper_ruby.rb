# frozen_string_literal: true

require "test_helper"
require "httpx"

class TestHyperRuby < Minitest::Test

  ACCEPT_RESPONSE = HyperRuby::Response.new(202, { 'Content-Type' => 'text/plain' }, '').freeze

  def test_that_it_has_a_version_number
    refute_nil ::HyperRuby::VERSION
  end

  def test_simple_get
    with_server(-> (request) { handler_simple(request) }) do |client|
      response = client.get("/")
      assert_equal 200, response.status
      assert_equal "text/plain", response.headers["content-type"]
      assert_equal 'GET', response.body
    end
  end

  def test_header_fetch_get
    with_server(-> (request) { handler_return_header(request, 'User-Agent') }) do |client|
      response = client.get("/", headers: { 'User-Agent' => 'test' })
      assert_equal 200, response.status
      assert_equal "test", JSON.parse(response.body)["message"]
    end
  end

  def test_simple_post
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_to_json(request, buffer) }) do |client|
      response = client.post("/", body: "Hello")
      assert_equal 200, response.status
      assert_equal "application/json", response.headers["content-type"]
      assert_equal 'Hello', JSON.parse(response.body)["message"]
    end
  end

  def test_large_post
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_to_json(request, buffer) }) do |client|
      response = client.post("/", body: "a" * 10_000_000)
      assert_equal 200, response.status
      assert_equal "application/json", response.headers["content-type"]
      assert_equal 'a' * 10_000_000, JSON.parse(response.body)["message"]
    end
  end

  def test_unix_socket_cleans_up_socket
    with_unix_socket_server(-> (request) { handler_simple(request) }) do |client|
      response = client.get("/")
      assert_equal 200, response.status
      assert_equal "text/plain", response.headers["content-type"]
      assert_equal 'GET', response.body
    end

    with_unix_socket_server(-> (request) { handler_simple(request) }) do |client|
      response = client.get("/")
      assert_equal 200, response.status
      assert_equal "text/plain", response.headers["content-type"]
      assert_equal 'GET', response.body
    end
  end

  def test_blocking
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_accept(request, buffer) }) do |client|
      gets
    end
  end

  def with_server(request_handler, &block)
    server = HyperRuby::Server.new
    server.configure({ bind_address: "127.0.0.1:3010",tokio_threads: 1 })
    server.start
    
    # Create ruby worker threads that process requests;
    # 1 is usually enough, and generally handles better than multiple threads 
    # if there's no IO (because of the GIL)
    worker = Thread.new do
      server.run_worker do |request|
        # Process the request in Ruby
        # request is a hash with :method, :path, :headers, and :body keys
        request_handler.call(request)
      end
    end

    client = HTTPX.with(origin: "http://127.0.0.1:3010")
    block.call(client)

  ensure
    server.stop if server
    worker.join if worker
  end

  def with_unix_socket_server(request_handler, &block)
    server = HyperRuby::Server.new
    server.configure({ bind_address: "unix:/tmp/hyper_ruby_test.sock", tokio_threads: 1 })
    server.start
    
    # Create ruby worker threads that process requests;
    # 1 is usually enough, and generally handles better than multiple threads 
    # if there's no IO (because of the GIL)
    worker = Thread.new do
      server.run_worker do |request|
        # Process the request in Ruby
        # request is a hash with :method, :path, :headers, and :body keys
        request_handler.call(request)
      end
    end

    client = HTTPX.with(transport: "unix", addresses: ["/tmp/hyper_ruby_test.sock"], origin: "http://host")
	
    block.call(client)

  ensure
    server.stop if server
    worker.join if worker
  end

  def handler_simple(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, request.http_method)
  end

  def handler_to_json(request, buffer)
    request.fill_body(buffer)
    HyperRuby::Response.new(200, { 'Content-Type' => 'application/json' }, { message: buffer }.to_json)
  end
  
  def handler_return_header(request, header_key)
    HyperRuby::Response.new(200, { 'Content-Type' => 'application/json' }, { message: request.header(header_key) }.to_json)
  end

  def handler_accept(request, buffer)
    request.fill_body(buffer)
    ACCEPT_RESPONSE
  end
end
