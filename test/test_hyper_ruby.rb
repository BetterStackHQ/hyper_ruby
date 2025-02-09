# frozen_string_literal: true

require "test_helper"
require "httpx"

class TestHyperRuby < Minitest::Test
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

  def test_simple_post
    with_server(-> (request) { handler_to_json(request) }) do |client|
      response = client.post("/", body: "Hello")
      assert_equal 200, response.status
      assert_equal "application/json", response.headers["content-type"]
      assert_equal 'Hello', JSON.parse(response.body)["message"]
    end
  end

  def test_large_post
    with_server(-> (request) { handler_to_json(request) }) do |client|
      response = client.post("/", body: "a" * 10_000_000)
      assert_equal 200, response.status
      assert_equal "application/json", response.headers["content-type"]
      assert_equal 'a' * 10_000_000, JSON.parse(response.body)["message"]
    end
  end

  def with_server(request_handler, &block)
    server = HyperRuby::Server.new
    server.configure({ bind_address: "127.0.0.1:3010" })
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

  def handler_simple(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, request.http_method)
  end

  def handler_to_json(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'application/json' }, { message: request.body }.to_json)
  end
end
