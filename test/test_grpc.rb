# frozen_string_literal: true

require "test_helper"
require 'zlib'
require_relative "echo_pb"
require_relative "echo_services_pb"


class TestGrpc < HyperRubyTest
  def test_grpc_request
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_grpc(request, buffer) }) do |_client|
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )
      
      request = Echo::EchoRequest.new(message: "Hello GRPC")
      response = stub.echo(request)
      
      assert_instance_of Echo::EchoResponse, response
      assert_equal "Hello GRPC response", response.message
    end
  end

  def test_concurrent_grpc_requests
    buffer = String.new(capacity: 1024)
    with_server(-> (request) { handler_grpc(request, buffer) }) do |_client|
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )
      
      threads = 5.times.map do |i|
        Thread.new do
          request = Echo::EchoRequest.new(message: "Hello GRPC #{i}")
          response = stub.echo(request)
          [i, response]
        end
      end

      responses = threads.map(&:value)
      responses.each do |i, response|
        assert_instance_of Echo::EchoResponse, response
        assert_equal "Hello GRPC #{i} response", response.message
      end
    end
  end

  def test_grpc_status_codes
    with_server(-> (request) { handler_grpc_status(request) }) do |_client|
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )

      # Test successful response (status 0)
      request = Echo::EchoRequest.new(message: "success")
      response = stub.echo(request)
      assert_equal "success response", response.message

      # Test error responses with different status codes
      {
        "invalid" => GRPC::InvalidArgument,
        "not_found" => GRPC::NotFound,
        "internal" => GRPC::Internal,
        "unimplemented" => GRPC::Unimplemented
      }.each do |message, expected_error|
        error = assert_raises(expected_error) do
          request = Echo::EchoRequest.new(message: message)
          stub.echo(request)
        end

        assert_equal "#{message} error", error.details
      end
    end
  end

  def test_request_type_detection
    with_server(-> (request) { handler_detect_type(request) }) do |client|
      # Test gRPC request
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

      # Test regular HTTP request
      http_response = client.post("/echo", body: "Hello HTTP")
      assert_equal 200, http_response.status
      assert_equal "text/plain", http_response.headers["content-type"]
      assert_equal "HTTP request: Hello HTTP", http_response.body
    end
  end

  def test_grpc_over_unix_socket
    buffer = String.new(capacity: 1024)
    with_unix_socket_server(-> (request) { handler_grpc(request, buffer) }) do |_client|
      # Create a gRPC channel using the Unix socket
      stub = Echo::Echo::Stub.new(
        "unix:///tmp/hyper_ruby_test.sock",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0,
          'grpc.default_authority' => 'localhost'  # Required for Unix socket
        }
      )
      
      request = Echo::EchoRequest.new(message: "Hello Unix Socket gRPC")
      response = stub.echo(request)
      
      assert_instance_of Echo::EchoResponse, response
      assert_equal "Hello Unix Socket gRPC response", response.message
    end
  end

  def test_grpc_compression
    buffer = String.new(capacity: 1024)
    compression_options = GRPC::Core::CompressionOptions.new(default_algorithm: :gzip)
    compression_channel_args = compression_options.to_channel_arg_hash

    with_server(-> (request) { handler_grpc_compressed(request, buffer) }) do |_client|
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0,
        }.merge(compression_channel_args)
      )

      request = Echo::EchoRequest.new(message: "Hello Compressed GRPC " + ("a" * 10000))
      response = stub.echo(request)

      assert_instance_of Echo::EchoResponse, response
      assert_equal "Decompressed: Hello Compressed GRPC " + ("a" * 10000), response.message
    end
  end

  def test_max_connection_age_sends_goaway
    # Test that max_connection_age causes the server to send GOAWAY after the configured time,
    # forcing the client to establish a new connection
    buffer = String.new(capacity: 1024)
    server_config = {
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      max_connection_age: 500  # 500ms max connection age
    }

    with_configured_server(server_config, -> (request) { handler_grpc(request, buffer) }) do |_client, server|
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )

      # Record initial connection count
      initial_connections = server.total_connections

      # First request establishes a connection
      request = Echo::EchoRequest.new(message: "Request 1")
      response = stub.echo(request)
      assert_equal "Request 1 response", response.message

      # Should have one connection now
      assert_equal initial_connections + 1, server.total_connections, "First request should establish one connection"

      # Wait for max_connection_age to expire and GOAWAY to be sent
      sleep 0.7

      # Make another request - gRPC client should establish a new connection after GOAWAY
      request = Echo::EchoRequest.new(message: "Request 2")
      response = stub.echo(request)
      assert_equal "Request 2 response", response.message

      # Should have a second connection now (client reconnected after GOAWAY)
      assert_equal initial_connections + 2, server.total_connections, "Second request after GOAWAY should establish a new connection"
    end
  end

  def test_long_max_connection_age_reuses_connection
    # Test that with a long max_connection_age, the connection is reused
    # (opposite of test_max_connection_age_sends_goaway)
    buffer = String.new(capacity: 1024)
    server_config = {
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      max_connection_age: 60000  # 60 seconds - much longer than test duration
    }

    with_configured_server(server_config, -> (request) { handler_grpc(request, buffer) }) do |_client, server|
      stub = Echo::Echo::Stub.new(
        "127.0.0.1:3010",
        :this_channel_is_insecure,
        channel_args: {
          'grpc.enable_http_proxy' => 0
        }
      )

      # Record initial connection count
      initial_connections = server.total_connections

      # First request establishes a connection
      request = Echo::EchoRequest.new(message: "Request 1")
      response = stub.echo(request)
      assert_equal "Request 1 response", response.message

      # Should have one connection now
      assert_equal initial_connections + 1, server.total_connections, "First request should establish one connection"

      # Wait a bit (but less than max_connection_age)
      sleep 0.3

      # Make another request - should reuse the same connection
      request = Echo::EchoRequest.new(message: "Request 2")
      response = stub.echo(request)
      assert_equal "Request 2 response", response.message

      # Should still have only one connection (connection reused, no GOAWAY sent)
      assert_equal initial_connections + 1, server.total_connections, "Second request should reuse existing connection"
    end
  end

  private

  def handler_grpc(request, buffer)
    assert_equal "application/grpc", request.header("content-type")
    assert_equal "echo.Echo", request.service
    assert_equal "Echo", request.method
    
    request.fill_body(buffer)
    echo_request = Echo::EchoRequest.decode(buffer)
    
    echo_response = Echo::EchoResponse.new(message: echo_request.message + " response")
    response_data = Echo::EchoResponse.encode(echo_response)
    
    HyperRuby::GrpcResponse.new(0, response_data)
  end

  def handler_detect_type(request)
    if request.is_a?(HyperRuby::GrpcRequest)
      buffer = String.new(capacity: 1024)
      request.fill_body(buffer)
      echo_request = Echo::EchoRequest.decode(buffer)
      
      echo_response = Echo::EchoResponse.new(message: "gRPC request: #{echo_request.message}")
      response_data = Echo::EchoResponse.encode(echo_response)
      
      HyperRuby::GrpcResponse.new(0, response_data)
    else
      buffer = String.new(capacity: 1024)
      request.fill_body(buffer)
      
      HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, "HTTP request: #{buffer}")
    end
  end

  def handler_grpc_status(request)
    buffer = String.new(capacity: 1024)
    request.fill_body(buffer)
    echo_request = Echo::EchoRequest.decode(buffer)
    
    case echo_request.message
    when "success"
      echo_response = Echo::EchoResponse.new(message: "success response")
      response_data = Echo::EchoResponse.encode(echo_response)
      HyperRuby::GrpcResponse.new(0, response_data)
    when "invalid"
      HyperRuby::GrpcResponse.error(3, "invalid error") # INVALID_ARGUMENT = 3
    when "not_found"
      HyperRuby::GrpcResponse.error(5, "not_found error") # NOT_FOUND = 5
    when "internal"
      HyperRuby::GrpcResponse.error(13, "internal error") # INTERNAL = 13
    when "unimplemented"
      HyperRuby::GrpcResponse.error(12, "unimplemented error") # UNIMPLEMENTED = 12
    else
      HyperRuby::GrpcResponse.error(2, "unknown error") # UNKNOWN = 2
    end
  end

  def handler_grpc_compressed(request, buffer)
    assert_equal "application/grpc", request.header("content-type")
    assert_equal "echo.Echo", request.service
    assert_equal "Echo", request.method
    # Check if the message is compressed
    assert request.compressed?, "Expected request to be compressed"
    
    # Get the compressed message
    request.fill_body(buffer)
    
    decompressed = Zlib.gunzip(buffer)
    echo_request = Echo::EchoRequest.decode(decompressed)
    
    echo_response = Echo::EchoResponse.new(message: "Decompressed: #{echo_request.message}")
    response_data = Echo::EchoResponse.encode(echo_response)
    
    HyperRuby::GrpcResponse.new(0, response_data)
  rescue => e
    pp e
    raise e
  end
end 