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
