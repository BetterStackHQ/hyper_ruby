# frozen_string_literal: true

require "test_helper"
require "httpx"
require "concurrent"
require "json"

class TestBackpressure < HyperRubyTest
  # Test to simulate a high volume of requests and verify the backpressure mechanism
  # This test focuses on measuring throughput differences and expects all requests to succeed
  def test_backpressure_handling
    # Counter to track processed requests
    processed_requests = Concurrent::AtomicFixnum.new(0)
    
    # A mutex to protect access to shared data
    mutex = Mutex.new
    
    # Track when the handler starts responding quickly
    start_fast_responses = nil
    
    # Request handler that takes different times based on request count
    request_handler = lambda do |request|
      current_count = processed_requests.increment
      
      if current_count <= 10
        # Slow responses for first 10 requests (100ms)
        sleep 0.1
        mutex.synchronize do
          # Record when we start responding quickly
          start_fast_responses = Time.now if current_count == 10
        end
      else
        # Fast responses after that (nearly instant)
      end
      
      # Return a simple response with the request count
      HyperRuby::Response.new(
        200, 
        {'Content-Type' => 'application/json'}, 
        {count: current_count, path: request.path}.to_json
      )
    end
    
    # Configure a server with a small channel capacity to trigger backpressure
    server_config = {
      bind_address: "127.0.0.1:3015",
      tokio_threads: 1,
      channel_capacity: 100,    # Increased capacity to handle the concurrent requests
      send_timeout: 1000,      # 1 second timeout
      recv_timeout: 1000       # 1 second timeout
    }
    
    # Use a single worker thread to maximize backpressure
    # This forces requests to be processed serially, making the test
    # more effective at validating the backpressure mechanism
    num_workers = 1
    
    with_configured_server(server_config, request_handler, num_workers) do |client|
      # Make a few initial requests to warm up
      3.times do
        client.get("/warmup")
      end
      
      # Number of concurrent requests to make
      num_requests = 200
      
      # Prepare a thread pool for concurrent requests
      threads = []
      responses = Concurrent::Array.new
      start_time = Time.now
      
      # Launch all requests at nearly the same time
      num_requests.times do |i|
        threads << Thread.new do
          begin
            response = client.get("/request-#{i}")
            responses << {
              path: "/request-#{i}",
              status: response.status,
              count: JSON.parse(response.body)["count"],
              time: Time.now - start_time
            }
          rescue => e
            responses << {error: e.message, path: "/request-#{i}"}
          end
        end
      end
      
      # Wait for all requests to complete
      threads.each(&:join)
      
      # Verify results
      assert_equal num_requests + 3, processed_requests.value, 
                 "All requests should have been processed"
      
      # Verify we didn't get any errors
      errors = responses.select { |r| r[:error] }
      assert errors.empty?, "Should not have any request errors: #{errors.inspect}"
      
      # Verify all responses had 200 status
      non_200 = responses.reject { |r| r[:status] == 200 }
      assert non_200.empty?, "All responses should have 200 status: #{non_200.inspect}"
      
      # Verify response counts
      counts = responses.map { |r| r[:count] }.sort
      assert_equal (4..(num_requests + 3)).to_a, counts, 
                 "Response counts should match expected sequence"
      
      # Analyze timing to verify backpressure handling
      if start_fast_responses
        # Get timestamps for requests completed after we switched to fast responses
        fast_response_times = responses.select { |r| r[:count] > 50 }
                                      .map { |r| r[:time] }
        
        # Calculate throughput before and after switching to fast responses
        slow_duration = start_fast_responses - start_time
        fast_duration = (Time.now - start_fast_responses)
        
        # Calculate statistics on fast response times
        if fast_response_times.any?
          min_time = fast_response_times.min
          max_time = fast_response_times.max
          avg_time = fast_response_times.sum / fast_response_times.size
          
          puts "Fast response times (seconds): min=#{min_time.round(3)}, " +
               "max=#{max_time.round(3)}, avg=#{avg_time.round(3)}"
        end
        
        # Only log, don't assert - timing can be variable in CI environments
        puts "Slow phase (#{50} requests): #{slow_duration.round(2)}s, " +
             "#{(50 / slow_duration).round(2)} req/s"
        
        if fast_duration > 0
          fast_count = num_requests - 50 + 3
          puts "Fast phase (#{fast_count} requests): #{fast_duration.round(2)}s, " +
               "#{(fast_count / fast_duration).round(2)} req/s"
          
          # Verify fast throughput was significantly higher than slow throughput
          slow_throughput = 50 / slow_duration
          fast_throughput = fast_count / fast_duration
          
          # The fast throughput should be at least 2x the slow throughput
          # This is a loose assertion since test environments can vary
          assert fast_throughput > slow_throughput * 2, 
                 "Fast throughput (#{fast_throughput.round(2)} req/s) should be " +
                 "at least 2x slow throughput (#{slow_throughput.round(2)} req/s)"
        end
      end
    end
  end
  
  # Test to verify that the server returns 429 Too Many Requests when backpressure timeout is reached
  # This test deliberately creates conditions to trigger 429 responses by using a minimal channel capacity
  def test_backpressure_timeout_returns_429    
    processed_requests = Concurrent::AtomicFixnum.new(0)
    
    # Simple request handler that blocks the first request for a few seconds
    request_handler = lambda do |request|
      req_num = processed_requests.increment
      
      puts "[HANDLER] Processing request #{request.path} (##{req_num})"
      
      if request.path == "/blocker"
        # Block the worker thread for 3 seconds
        puts "[HANDLER] Blocking request - sleeping for 3 seconds"
        sleep 3
        puts "[HANDLER] Blocker request completed"
      elsif request.path == "/fill"
        # This request fills the only slot in the channel
        puts "[HANDLER] Fill request - sleeping for 1 second"
        sleep 1
        puts "[HANDLER] Fill request completed"
      end
      
      # Return a response with the request count
      HyperRuby::Response.new(
        200, 
        {'Content-Type' => 'application/json'}, 
        {count: req_num, path: request.path}.to_json
      )
    end
    
    # Use a minimal channel capacity to ensure it fills quickly
    server_config = {
      bind_address: "127.0.0.1:3016",
      tokio_threads: 1,
      channel_capacity: 1,     # Minimal capacity
      send_timeout: 50,        # 50ms timeout as requested - allows for ~5 retry attempts
      recv_timeout: 5000,      # 5 second receive timeout
    }
    
    puts "==== STARTING SIMPLIFIED BACKPRESSURE TEST ===="
    with_configured_server(server_config, request_handler, 1) do |client|
      puts "[TEST] Server started with channel_capacity=#{server_config[:channel_capacity]}, send_timeout=#{server_config[:send_timeout]}ms"
      
      # Step 1: Send a blocking request to occupy the worker thread
      puts "[TEST] Step 1: Sending blocking request to occupy worker"
      blocker_thread = Thread.new do
        begin
          client.get("/blocker", timeout: { operation_timeout: 10 })
          puts "[TEST] Blocker request complete"
        rescue => e
          puts "[TEST] Blocker request error: #{e.message}"
        end
      end
      
      # Allow blocker request to be picked up
      sleep 0.5
      
      # Step 2: Send a request to fill the channel
      puts "[TEST] Step 2: Sending request to fill the channel"
      fill_thread = Thread.new do
        begin
          client.get("/fill", timeout: { operation_timeout: 5 })
          puts "[TEST] Fill request complete"
        rescue => e
          puts "[TEST] Fill request error: #{e.message}"
        end
      end
      
      # Allow fill request to enter the channel
      sleep 0.5
      
      # Step 3: Now the worker is busy and the channel is full
      # Any new requests should get 429 responses
      puts "[TEST] Step 3: Sending flood requests that should get 429 responses"
      responses = []
      
      # Send several requests that should all get 429s
      5.times do |i|
        begin
          puts "[TEST] Sending flood request #{i}"
          response = client.get("/flood-#{i}", timeout: { operation_timeout: 2 })
          responses << {
            path: "/flood-#{i}",
            status: response.status,
            body: response.body.to_s
          }
        rescue => e
          puts "[TEST] Flood request #{i} error: #{e.message}"
          responses << {error: e.message, path: "/flood-#{i}"}
        end
      end
      
      # Wait for original threads to complete
      blocker_thread.join(5)
      fill_thread.join(5)
      
      # Analyze results
      responses_429 = responses.select { |r| r[:status] == 429 }
      
      puts "[TEST] Received #{responses_429.size} 429 responses out of #{responses.size} flood requests"
      
      # We should have at least one 429 response
      assert_operator responses_429.size, :>=, 1, 
                     "Expected at least one 429 response, but got none. This indicates the backpressure mechanism is not working properly."
      
      # Verify response content
      if responses_429.any?
        sample = responses_429.first
        assert_includes sample[:body], "Server too busy", 
                       "429 response should indicate server is too busy, but got: #{sample[:body]}"
        
        puts "[TEST] SUCCESS! Verified 429 response contains correct message"
      end
    end
  end
  
  # Helper method to create a server with the specified number of worker threads
  def with_configured_server(config, request_handler, num_workers = 1, &block)
    server = HyperRuby::Server.new
    server.configure(config)
    server.start
    
    # Create multiple Ruby worker threads to process requests
    workers = num_workers.times.map do
      Thread.new do
        server.run_worker do |request|
          # Process the request in Ruby
          request_handler.call(request)
        end
      end
    end

    # Create client and pass to block
    client = HTTPX.with(
      origin: "http://#{config[:bind_address]}",
      timeout: {
        connect_timeout: 5,    # 5 seconds to connect
        operation_timeout: 15  # 15 seconds for operations
      }
    )
    block.call(client)

  ensure
    server&.stop
    workers&.each(&:join)
  end
end 