# frozen_string_literal: true

require "test_helper"
require "httpx"

class TestHttp < HyperRubyTest
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

  def test_headers_fetch_all
    with_server(-> (request) { handler_return_all_headers(request) }) do |client|
      response = client.get("/", headers: { 
        'User-Agent' => 'test',
        'X-Custom-Header' => 'custom',
        'Accept' => 'application/json'
      })
      assert_equal 200, response.status
      headers = JSON.parse(response.body)["headers"]
      assert_equal 'test', headers['user-agent']
      assert_equal 'custom', headers['x-custom-header']
      assert_equal 'application/json', headers['accept']
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

  def test_unix_socket
    with_unix_socket_server(-> (request) { handler_simple(request) }) do |client|
      response = client.get("/")
      assert_equal 200, response.status
      assert_equal "text/plain", response.headers["content-type"]
      assert_equal 'GET', response.body
    end
  end

  def test_options
    with_server(-> (request) { handler_simple(request) }) do |client|
      response = client.options("/", headers: { 'User-Agent' => 'test', 'Origin' => 'http://example.com' })
      assert_equal 200, response.status
    end
  end

  def test_head
    with_server(-> (request) { handler_simple(request) }) do |client|
      response = client.head("/", headers: { 'User-Agent' => 'test', 'Origin' => 'http://example.com' })
      assert_equal 200, response.status
      assert_equal '', response.body.to_s
    end
  end

  def test_http2_request
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_to_json(request, buffer) }) do |client|
      # Configure client for HTTP/2
      client = client.with(
        debug: STDERR,
        debug_level: 3,
        fallback_protocol: "h2"
      )
      
      # Send a simple POST request
      response = client.post(
        "/",
        headers: {
          "content-type" => "application/json",
          "accept" => "application/json"
        },
        body: { "message" => "Hello HTTP/2" }.to_json
      )
      
      assert_equal 200, response.status
      assert_equal "application/json", response.headers["content-type"]
      assert_equal({ "message" => { "message" => "Hello HTTP/2" }.to_json }, JSON.parse(response.body))
      assert_equal "2.0", response.version
    end
  end

  private

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

  def handler_return_all_headers(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'application/json' }, { headers: request.headers }.to_json)
  end

  def handler_dump_request(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, "")
  end

  def handler_accept(request, buffer)
    request.fill_body(buffer)
    ACCEPT_RESPONSE
  end
end
