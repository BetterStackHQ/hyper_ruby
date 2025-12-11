# frozen_string_literal: true

require "test_helper"
require "httpx"
require "fileutils"

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

  def test_address_binding_error
    # First server binds to the port
    server1 = HyperRuby::Server.new
    server1.configure({ bind_address: "127.0.0.1:3020" })
    server1.start

    begin
      # Try to start a second server on the same port, which should fail
      server2 = HyperRuby::Server.new
      server2.configure({ bind_address: "127.0.0.1:3020" })
      
      # This should raise an exception
      error = assert_raises(RuntimeError) do
        server2.start
      end
      
      # Verify that the error message contains information about the binding failure
      assert_match(/Failed to bind to address/, error.message)
    ensure
      # Clean up
      server1.stop if server1
    end
  end

  def test_unix_socket_cleanup
    # Create a temporary path for the socket
    socket_path = "/tmp/hyper_ruby_test_cleanup.sock"
    
    # Clean up any leftover socket file from previous test runs
    File.unlink(socket_path) if File.exist?(socket_path)
    
    # First ensure that automatic cleanup works when a socket exists
    # but is deletable - this should work without errors
    FileUtils.touch(socket_path)
    File.chmod(0644, socket_path)  # Ensure we have permissions
    
    begin
      server = HyperRuby::Server.new
      server.configure({ bind_address: "unix:#{socket_path}" })
      server.start
      
      # If we get here, the server started correctly
      assert File.exist?(socket_path), "Socket file should exist after server starts"
    ensure
      server.stop if server
      File.unlink(socket_path) if File.exist?(socket_path)
    end
  end

  # This test requires root permissions to create a file that can't be deleted.
  # Skip it unless we're running with proper permissions.
  def test_unix_socket_undeletable
    # Skip if we're not root or can't modify file permissions
    skip unless Process.uid == 0 || system("sudo -n true 2>/dev/null")
    
    socket_path = "/tmp/hyper_ruby_test_undeletable.sock"
    
    # Clean up any leftover socket file
    File.unlink(socket_path) if File.exist?(socket_path)
    
    # Create a file at the socket path that can't be deleted by the current user
    system("sudo touch #{socket_path} && sudo chmod 0000 #{socket_path}")
    
    begin
      server = HyperRuby::Server.new
      server.configure({ bind_address: "unix:#{socket_path}" })
      
      # This should raise an exception about not being able to remove the file
      error = assert_raises(RuntimeError) do
        server.start
      end
      
      # Verify the error message
      assert_match(/Failed to remove existing Unix socket file/, error.message)
    ensure
      # Clean up with sudo
      system("sudo rm -f #{socket_path}") if File.exist?(socket_path)
    end
  end

  def test_unix_socket_directory_error
    # Create a directory instead of a socket file
    socket_dir = "/tmp/hyper_ruby_test_dir"
    
    # Ensure the directory exists
    FileUtils.mkdir_p(socket_dir)
    
    # Try to bind to the directory (which should fail)
    begin
      server = HyperRuby::Server.new
      server.configure({ bind_address: "unix:#{socket_dir}" })
      
      # This should raise an exception
      error = assert_raises(RuntimeError) do
        server.start
      end
      
      # The error is from trying to remove the directory, not from binding
      assert_match(/Failed to remove existing Unix socket file/, error.message)
      
      # It should include something about "Operation not permitted" or similar
      assert(error.message.include?("Operation not permitted") || 
             error.message.include?("Permission denied") || 
             error.message.include?("not a socket"), 
             "Error should indicate issue with removing directory: #{error.message}")
    ensure
      # Clean up
      FileUtils.rm_rf(socket_dir) if Dir.exist?(socket_dir)
    end
  end

  def test_query_params
    with_server(-> (request) { handler_query_params(request) }) do |client|
      response = client.get("/?name=John&age=30&verified")
      assert_equal 200, response.status
      
      data = JSON.parse(response.body)
      assert_equal "John", data["query_params"]["name"]
      assert_equal "30", data["query_params"]["age"]
      assert_equal "", data["query_params"]["verified"]
      
      assert_equal "John", data["query_param_name"]
      assert_nil data["query_param_missing"]
    end
  end
  
  def test_query_params_with_encoding
    with_server(-> (request) { handler_query_params(request) }) do |client|
      response = client.get("/?q=hello%20world&filter=type%3Duser")
      assert_equal 200, response.status
      
      data = JSON.parse(response.body)
      assert_equal "hello world", data["query_params"]["q"]
      assert_equal "type=user", data["query_params"]["filter"]
    end
  end

  def test_host
    with_server(-> (request) { handler_return_host(request) }) do |client|
      response = client.get("/")
      assert_equal 200, response.status
      # The Host header should contain the server address
      host_header = JSON.parse(response.body)["message"]
      assert_match(/127\.0\.0\.1/, host_header)
    end
  end

  def test_http2_host
    with_server(-> (request) { handler_return_host(request) }) do |client|
      # Configure client for HTTP/2
      client = client.with(
        debug: STDERR,
        debug_level: 3,
        fallback_protocol: "h2"
      )
      
      response = client.get("/")
      assert_equal 200, response.status
      assert_equal "2.0", response.version
      
      # The Host header should contain the server address
      host_header = JSON.parse(response.body)["message"]
      assert_match(/127\.0\.0\.1/, host_header)
    end
  end

  def test_chunked_transfer_encoding
    buffer = String.new(capacity: 65536)
    with_server(-> (request) { handler_to_json(request, buffer) }) do |_client|
      require 'socket'
      
      # Create a large body (64KB) to ensure chunked encoding is meaningful
      chunk1 = "A" * 16384  # 16KB
      chunk2 = "B" * 16384  # 16KB
      chunk3 = "C" * 16384  # 16KB
      chunk4 = "D" * 16384  # 16KB
      expected_body = chunk1 + chunk2 + chunk3 + chunk4
      
      # Build a raw HTTP/1.1 request with Transfer-Encoding: chunked
      socket = TCPSocket.new("127.0.0.1", 3010)
      
      # Send headers
      socket.write("POST / HTTP/1.1\r\n")
      socket.write("Host: 127.0.0.1:3010\r\n")
      socket.write("Transfer-Encoding: chunked\r\n")
      socket.write("Content-Type: text/plain\r\n")
      socket.write("Connection: close\r\n")
      socket.write("\r\n")
      
      # Send body in chunks (chunked encoding format: size in hex, CRLF, data, CRLF)
      [chunk1, chunk2, chunk3, chunk4].each do |chunk|
        socket.write("#{chunk.bytesize.to_s(16)}\r\n")
        socket.write(chunk)
        socket.write("\r\n")
      end
      
      # Send final zero-length chunk to signal end
      socket.write("0\r\n")
      socket.write("\r\n")
      
      # Read response
      response = socket.read
      socket.close
      
      # Parse response - find the JSON body after headers
      headers_end = response.index("\r\n\r\n")
      assert headers_end, "Response should have headers"
      
      status_line = response.lines.first
      assert_match(/HTTP\/1\.1 200/, status_line, "Should receive 200 OK")
      
      # Extract body after headers
      body_part = response[(headers_end + 4)..]
      
      # Parse the JSON response and verify the body was correctly received
      json_response = JSON.parse(body_part)
      received_body = json_response["message"]
      
      assert_equal expected_body.bytesize, received_body.bytesize,
        "Body size should match (expected #{expected_body.bytesize}, got #{received_body.bytesize})"
      assert_equal expected_body, received_body, "Body content should match"
    end
  end

  def test_chunked_transfer_encoding_aborted_connection
    # Test that when a client aborts (RST) mid-chunked-transfer,
    # the partial body is NOT exposed to the handler
    handler_called = false

    server_config = {
      bind_address: "127.0.0.1:3010",
      tokio_threads: 1,
      recv_timeout: 1000  # 1 second timeout
    }

    with_configured_server(server_config, -> (request) {
      handler_called = true
      buffer = String.new(capacity: 65536)
      request.fill_body(buffer)
      HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, buffer)
    }) do |_client|
      chunk1 = "A" * 16384  # 16KB
      chunk2 = "B" * 16384  # 16KB

      # Create socket and set SO_LINGER to 0 to send RST on close (abort)
      socket = Socket.new(Socket::AF_INET, Socket::SOCK_STREAM)
      socket.setsockopt(Socket::SOL_SOCKET, Socket::SO_LINGER, [1, 0].pack("ii"))
      socket.connect(Socket.sockaddr_in(3010, "127.0.0.1"))

      # Send headers
      socket.write("POST / HTTP/1.1\r\n")
      socket.write("Host: 127.0.0.1:3010\r\n")
      socket.write("Transfer-Encoding: chunked\r\n")
      socket.write("Content-Type: text/plain\r\n")
      socket.write("Connection: close\r\n")
      socket.write("\r\n")

      # Send first chunk successfully
      socket.write("#{chunk1.bytesize.to_s(16)}\r\n")
      socket.write(chunk1)
      socket.write("\r\n")

      # Send partial second chunk
      socket.write("#{chunk2.bytesize.to_s(16)}\r\n")
      socket.write(chunk2[0, 8192])  # Only half of chunk2

      # Abort the connection with RST (no graceful close)
      socket.close

      # Wait for the server to process and timeout
      sleep 1.5

      # The handler should NOT have been called since the chunked body was incomplete
      refute handler_called, "Handler should not be called when chunked transfer is aborted mid-stream"
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

  def handler_return_host(request)
    HyperRuby::Response.new(200, { 'Content-Type' => 'application/json' }, { message: request.host() }.to_json)
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

  def handler_query_params(request)
    response_data = {
      query_params: request.query_params,
      query_param_name: request.query_param("name"),
      query_param_missing: request.query_param("missing")
    }
    
    HyperRuby::Response.new(
      200,
      { 'Content-Type' => 'application/json' },
      JSON.generate(response_data)
    )
  end
end
