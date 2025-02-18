# frozen_string_literal: true

require "test_helper"
require "httpx"
require_relative "echo_pb"
require_relative "echo_services_pb"

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

  # def test_large_post
  #   buffer = String.new(capacity: 1024)
  #   with_server(-> (request) { handler_to_json(request, buffer) }) do |client|
  #     response = client.post("/", body: "a" * 10_000_000)
  #     assert_equal 200, response.status
  #     assert_equal "application/json", response.headers["content-type"]
  #     assert_equal 'a' * 10_000_000, JSON.parse(response.body)["message"]
  #   end
  # end

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

  # test OPTIONS and HEAD methods
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

  def test_blocking
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_accept(request, buffer) }) do |client|
      gets
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

  def test_grpc_request
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_grpc(request, buffer) }) do |_client|
      # Create a gRPC stub using the standard Ruby gRPC client
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )
      
      # Create request message
      request = Echo::EchoRequest.new(message: "Hello GRPC")
      
      # Make the gRPC call
      response = stub.echo(request)
      
      # Check the response
      assert_instance_of Echo::EchoResponse, response
      assert_equal "Hello GRPC response", response.message
    end
  end

  def test_concurrent_grpc_requests
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_grpc(request, buffer) }) do |_client|
      # Create a gRPC stub using the standard Ruby gRPC client
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )
      
      # Create multiple threads to send requests concurrently
      threads = 5.times.map do |i|
        Thread.new do
          request = Echo::EchoRequest.new(message: "Hello GRPC #{i}")
          response = stub.echo(request)
          [i, response]
        end
      end

      # Collect and verify all responses
      responses = threads.map(&:value)
      responses.each do |i, response|
        assert_instance_of Echo::EchoResponse, response
        assert_equal "Hello GRPC #{i} response", response.message
      end
    end
  end

  def test_request_type_detection
    with_server(-> (request) { handler_detect_type(request) }) do |client|
      # Test regular HTTP request
      http_response = client.post("/echo", body: "Hello HTTP")
      assert_equal 200, http_response.status
      assert_equal "text/plain", http_response.headers["content-type"]
      assert_equal "HTTP request: Hello HTTP", http_response.body

      # Test gRPC request using the gRPC client
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )
      
      request = Echo::EchoRequest.new(message: "Hello gRPC")
      grpc_response = stub.echo(request)
      
      assert_instance_of Echo::EchoResponse, grpc_response
      assert_equal "gRPC request: Hello gRPC", grpc_response.message
    end
  end

  def with_server(request_handler, &block)
    server = HyperRuby::Server.new
    server.configure({ 
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      #debug: true 
    })
    server.start
    
    # Create ruby worker threads that process requests;
    # 1 is usually enough, and generally handles better than multiple threads 
    # if there's no IO (because of the GIL)
    workers = 1.times.map do
      Thread.new do
        server.run_worker do |request|
          # Process the request in Ruby
          # request is a hash with :method, :path, :headers, and :body keys
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
    
    # Create ruby worker threads that process requests;
    # 1 is usually enough, and generally handles better than multiple threads 
    # if there's no IO (because of the GIL)
    workers = 2.times.map do
      Thread.new do
        server.run_worker do |request|
          # Process the request in Ruby
          # request is a hash with :method, :path, :headers, and :body keys
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

  def handler_grpc(request, buffer)
    assert_equal "application/grpc", request.header("content-type")
    assert_equal "echo.Echo", request.service
    assert_equal "Echo", request.method
    
    # Decode the request protobuf
    request.fill_body(buffer)
    echo_request = Echo::EchoRequest.decode(buffer)
    
    # Create and encode the response protobuf
    echo_response = Echo::EchoResponse.new(message: echo_request.message + " response")
    response_data = Echo::EchoResponse.encode(echo_response)
    
    # Return gRPC response
    HyperRuby::GrpcResponse.new(0, response_data)
  end

  def handler_detect_type(request)
    if request.is_a?(HyperRuby::GrpcRequest)
      # Handle gRPC request
      buffer = String.new(capacity: 1024)
      request.fill_body(buffer)
      echo_request = Echo::EchoRequest.decode(buffer)
      
      echo_response = Echo::EchoResponse.new(message: "gRPC request: #{echo_request.message}")
      response_data = Echo::EchoResponse.encode(echo_response)
      
      HyperRuby::GrpcResponse.new(0, response_data)
    else
      # Handle regular HTTP request
      buffer = String.new(capacity: 1024)
      request.fill_body(buffer)
      
      HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, "HTTP request: #{buffer}")
    end
  end
end
