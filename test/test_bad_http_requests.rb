# frozen_string_literal: true

require "test_helper"
require "httpx"
require "socket"

class TestBadHttpRequests < HyperRubyTest
  def test_oversized_headers
    with_server(-> (request) { handler_simple(request) }) do |client|
      # Create a large but reasonable header
      large_header = "x" * 8 * 1024  # 8KB header
      response = client.get("/", headers: { "X-Large-Header" => large_header })
      assert_equal 200, response.status  # Server handles large headers
      
      # Test with multiple headers
      many_headers = (1..50).map { |i| ["X-Header-#{i}", "value"] }.to_h
      response = client.get("/", headers: many_headers)
      assert_equal 200, response.status  # Server handles many headers
    end
  end

  def test_malformed_headers
    with_server(-> (request) { handler_simple(request) }) do |client|
      # Test with invalid header values that might make it through to the server
      response = client.get("/", headers: { "X-Header" => "value\ninjection" })
      assert_equal 400, response.status  # Server rejects request
    end
  end

  def test_oversized_requests
    with_server(-> (request) { handler_simple(request) }) do |client|
      # Test with a large but reasonable body
      large_body = "x" * 1 * 1024 * 1024  # 1MB body
      response = client.post("/", body: large_body)
      assert_equal 200, response.status  # Server handles large bodies
    end
  end

  def test_mismatched_content_length
    server_config = {
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      recv_timeout: 1000  # 1 second timeout
    }

    with_configured_server(server_config, -> (request) { handler_echo(request) }) do |_client|
      test_body = "test body content"

      # Test with Content-Length larger than actual content and incomplete body
      socket = TCPSocket.new("127.0.0.1", 3010)
      request_headers = <<~HEADERS
        POST / HTTP/1.1
        Host: 127.0.0.1:3010
        Content-Length: 1000
        Connection: close

      HEADERS
      request_headers = request_headers.gsub("\n", "\r\n")
      
      socket.write(request_headers)
      socket.write(test_body)  # Only send a small body
      
      # Server should timeout after 1 second
      response = read_http_response(socket)
      socket.close
      
      assert_equal 408, response[:status].split(" ")[1].to_i  # Request Timeout
      assert_match(/timed out while receiving body/i, response[:body].to_s)  # Body should mention timeout

      # Test with Content-Length smaller than sent data
      socket = TCPSocket.new("127.0.0.1", 3010)
      request_headers = <<~HEADERS
        POST / HTTP/1.1
        Host: 127.0.0.1:3010
        Content-Length: 1
        Connection: close

      HEADERS
      request_headers = request_headers.gsub("\n", "\r\n")
      
      socket.write(request_headers)
      socket.write(test_body[0])  # Send exactly 1 byte as specified in Content-Length
      response = read_http_response(socket)
      socket.close
      
      assert_equal 200, response[:status].split(" ")[1].to_i
      assert_equal test_body[0,1], response[:body]  # Should only get the first byte back
    end
  end

  def test_header_timeout
    server_config = {
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      recv_timeout: 1000  # 1 second timeout
    }

    with_configured_server(server_config, -> (request) { handler_echo(request) }) do |_client|
      socket = TCPSocket.new("127.0.0.1", 3010)
      
      # Send first line of headers
      socket.write("POST / HTTP/1.1\r\n")
      socket.write("Host: 127.0.0.1:3010\r\n")
      
      # Sleep longer than the timeout
      sleep 1.5
      begin
        # Try to send the rest of the headers, but the connection should be closed
        socket.write("Content-Length: 0\r\n")
        socket.write("Connection: close\r\n")
        socket.write("\r\n")
        
        # Attempt to read response - should be a timeout or connection closed
        response = read_http_response(socket)
        socket.close
        
        # The server might respond with a 408 timeout, or might just close the connection
        # Both behaviors are acceptable according to HTTP/1.1 spec
        if response[:status]
          assert_equal 408, response[:status].split(" ")[1].to_i  # Request Timeout if we got a response
        end
      rescue Errno::EPIPE
      # This is expected if the server closes the connection due to timeout/error
      # This is not an error, so we don't need to assert anything
      end
    end
  end

  private

  def handler_simple(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, request.http_method)
  end

  def handler_echo(request)
    buffer = String.new(capacity: 1024)
    request.fill_body(buffer)
    HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, buffer)
  end

  def read_http_response(socket)
    response = { headers: {}, body: "" }
    
    # Read status line with timeout
    response[:status] = socket.gets.strip
    
    # Read headers
    while (line = socket.gets.strip) != ""
      name, value = line.split(": ", 2)
      response[:headers][name.downcase] = value
    end
    
    # Read body if present
    if response[:headers]["content-length"]
      response[:body] = socket.read(response[:headers]["content-length"].to_i)
    else
      response[:body] = socket.read
    end
    
    response
  rescue IOError, Errno::ECONNRESET, Errno::EPIPE, NoMethodError
    # If the server closes the connection due to timeout/error,
    # return what we have so far; nomethod error can be triggered by a nil value from a socket read
    response
  end
end 