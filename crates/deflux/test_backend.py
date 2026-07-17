#!/usr/bin/env python3
"""
Test backend server for Deflux load balancer testing.
Provides configurable HTTP servers with health checks, metrics, and load simulation.
"""

import asyncio
import argparse
import json
import time
import random
import signal
import sys
from datetime import datetime
from aiohttp import web, ClientSession
import aiohttp_cors
import logging

class TestBackend:
    def __init__(self, port, name=None, delay_ms=0, error_rate=0.0, health=True):
        self.port = port
        self.name = name or f"backend-{port}"
        self.delay_ms = delay_ms
        self.error_rate = error_rate
        self.healthy = health
        self.start_time = time.time()
        self.request_count = 0
        self.error_count = 0
        
        # Setup logging
        logging.basicConfig(
            level=logging.INFO,
            format=f'[{self.name}] %(asctime)s - %(levelname)s - %(message)s'
        )
        self.logger = logging.getLogger(self.name)
        
        # Create app
        self.app = web.Application()
        self.setup_routes()
        self.setup_cors()
        
    def setup_routes(self):
        """Setup HTTP routes"""
        self.app.router.add_get('/', self.handle_root)
        self.app.router.add_get('/health', self.handle_health)
        self.app.router.add_get('/metrics', self.handle_metrics)
        self.app.router.add_get('/info', self.handle_info)
        self.app.router.add_get('/slow', self.handle_slow)
        self.app.router.add_get('/error', self.handle_error)
        self.app.router.add_post('/toggle-health', self.handle_toggle_health)
        self.app.router.add_post('/set-delay/{delay}', self.handle_set_delay)
        self.app.router.add_post('/set-error-rate/{rate}', self.handle_set_error_rate)
        
        # Catch-all for any other paths
        self.app.router.add_route('*', '/{path:.*}', self.handle_default)
    
    def setup_cors(self):
        """Setup CORS for cross-origin requests"""
        cors = aiohttp_cors.setup(self.app, defaults={
            "*": aiohttp_cors.ResourceOptions(
                allow_credentials=True,
                expose_headers="*",
                allow_headers="*",
                allow_methods="*"
            )
        })
        
        # Add CORS to all routes
        for route in list(self.app.router.routes()):
            cors.add(route)
    
    async def simulate_processing(self):
        """Simulate processing delay"""
        if self.delay_ms > 0:
            await asyncio.sleep(self.delay_ms / 1000.0)
    
    def should_error(self):
        """Check if this request should return an error"""
        return random.random() < self.error_rate
    
    async def handle_root(self, request):
        """Handle root path requests"""
        self.request_count += 1
        await self.simulate_processing()
        
        if self.should_error():
            self.error_count += 1
            raise web.HTTPInternalServerError(text=f"Simulated error from {self.name}")
        
        response_data = {
            "backend": self.name,
            "port": self.port,
            "timestamp": datetime.now().isoformat(),
            "request_id": self.request_count,
            "client_ip": request.remote,
            "user_agent": request.headers.get('User-Agent', 'Unknown'),
            "method": request.method,
            "path": request.path_qs,
        }
        
        self.logger.info(f"Request {self.request_count}: {request.method} {request.path_qs}")
        
        return web.json_response(response_data)
    
    async def handle_health(self, request):
        """Health check endpoint"""
        if not self.healthy:
            self.logger.warning("Health check failed - marked unhealthy")
            raise web.HTTPServiceUnavailable(text=f"{self.name} is unhealthy")
        
        health_data = {
            "status": "healthy",
            "backend": self.name,
            "port": self.port,
            "uptime": time.time() - self.start_time,
            "timestamp": datetime.now().isoformat()
        }
        
        return web.json_response(health_data)
    
    async def handle_metrics(self, request):
        """Metrics endpoint"""
        uptime = time.time() - self.start_time
        
        metrics_data = {
            "backend": self.name,
            "port": self.port,
            "uptime_seconds": uptime,
            "total_requests": self.request_count,
            "error_count": self.error_count,
            "success_count": self.request_count - self.error_count,
            "error_rate": self.error_count / max(self.request_count, 1),
            "requests_per_second": self.request_count / max(uptime, 1),
            "current_delay_ms": self.delay_ms,
            "current_error_rate": self.error_rate,
            "healthy": self.healthy,
            "timestamp": datetime.now().isoformat()
        }
        
        return web.json_response(metrics_data)
    
    async def handle_info(self, request):
        """Backend information endpoint"""
        info_data = {
            "backend": self.name,
            "port": self.port,
            "version": "1.0.0",
            "description": "Test backend server for Deflux load balancer",
            "features": [
                "health_checks",
                "metrics",
                "configurable_delay",
                "configurable_error_rate",
                "cors_enabled"
            ],
            "endpoints": [
                "/",
                "/health",
                "/metrics", 
                "/info",
                "/slow",
                "/error",
                "POST /toggle-health",
                "POST /set-delay/{delay}",
                "POST /set-error-rate/{rate}"
            ]
        }
        
        return web.json_response(info_data)
    
    async def handle_slow(self, request):
        """Endpoint that always has slow response"""
        self.request_count += 1
        slow_delay = max(self.delay_ms, 2000)  # At least 2 seconds
        
        self.logger.info(f"Slow request - adding {slow_delay}ms delay")
        await asyncio.sleep(slow_delay / 1000.0)
        
        return web.json_response({
            "backend": self.name,
            "message": f"Slow response with {slow_delay}ms delay",
            "timestamp": datetime.now().isoformat()
        })
    
    async def handle_error(self, request):
        """Endpoint that always returns an error"""
        self.request_count += 1
        self.error_count += 1
        
        self.logger.warning("Error endpoint called - returning 500")
        raise web.HTTPInternalServerError(text=f"Intentional error from {self.name}")
    
    async def handle_toggle_health(self, request):
        """Toggle health status"""
        self.healthy = not self.healthy
        status = "healthy" if self.healthy else "unhealthy"
        
        self.logger.info(f"Health status toggled to: {status}")
        
        return web.json_response({
            "backend": self.name,
            "action": "toggle_health",
            "new_status": status,
            "healthy": self.healthy
        })
    
    async def handle_set_delay(self, request):
        """Set response delay"""
        try:
            delay = int(request.match_info['delay'])
            old_delay = self.delay_ms
            self.delay_ms = max(0, delay)
            
            self.logger.info(f"Delay changed from {old_delay}ms to {self.delay_ms}ms")
            
            return web.json_response({
                "backend": self.name,
                "action": "set_delay",
                "old_delay_ms": old_delay,
                "new_delay_ms": self.delay_ms
            })
        except ValueError:
            raise web.HTTPBadRequest(text="Invalid delay value")
    
    async def handle_set_error_rate(self, request):
        """Set error rate"""
        try:
            rate = float(request.match_info['rate'])
            old_rate = self.error_rate
            self.error_rate = max(0.0, min(1.0, rate))
            
            self.logger.info(f"Error rate changed from {old_rate:.2%} to {self.error_rate:.2%}")
            
            return web.json_response({
                "backend": self.name,
                "action": "set_error_rate",
                "old_error_rate": old_rate,
                "new_error_rate": self.error_rate
            })
        except ValueError:
            raise web.HTTPBadRequest(text="Invalid error rate value")
    
    async def handle_default(self, request):
        """Handle all other requests"""
        self.request_count += 1
        await self.simulate_processing()
        
        if self.should_error():
            self.error_count += 1
            raise web.HTTPInternalServerError(text=f"Simulated error from {self.name}")
        
        response_data = {
            "backend": self.name,
            "port": self.port,
            "message": f"Default handler for {request.method} {request.path_qs}",
            "timestamp": datetime.now().isoformat(),
            "request_id": self.request_count
        }
        
        return web.json_response(response_data)
    
    async def start(self):
        """Start the backend server"""
        runner = web.AppRunner(self.app)
        await runner.setup()
        
        site = web.TCPSite(runner, '0.0.0.0', self.port)
        await site.start()
        
        self.logger.info(f"Backend server started on port {self.port}")
        self.logger.info(f"Configuration: delay={self.delay_ms}ms, error_rate={self.error_rate:.2%}, healthy={self.healthy}")
        
        return runner

async def run_backend(port, name, delay_ms, error_rate, unhealthy):
    """Run a single backend server"""
    backend = TestBackend(
        port=port,
        name=name,
        delay_ms=delay_ms,
        error_rate=error_rate,
        health=not unhealthy
    )
    
    runner = await backend.start()
    
    # Setup signal handlers for graceful shutdown
    def signal_handler(signum, frame):
        print(f"\n[{backend.name}] Received signal {signum}, shutting down...")
        asyncio.create_task(runner.cleanup())
        sys.exit(0)
    
    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)
    
    # Keep running
    try:
        while True:
            await asyncio.sleep(1)
    except KeyboardInterrupt:
        print(f"\n[{backend.name}] Keyboard interrupt received, shutting down...")
    finally:
        await runner.cleanup()

async def run_multiple_backends(ports, base_name, delay_ms, error_rate, unhealthy):
    """Run multiple backend servers concurrently"""
    backends = []
    
    for i, port in enumerate(ports):
        name = f"{base_name}-{i+1}" if len(ports) > 1 else base_name
        backend = TestBackend(
            port=port,
            name=name,
            delay_ms=delay_ms,
            error_rate=error_rate,
            health=not unhealthy
        )
        backends.append(backend)
    
    # Start all backends
    runners = []
    for backend in backends:
        runner = await backend.start()
        runners.append(runner)
    
    print(f"\nAll {len(backends)} backend servers started successfully!")
    print("Available endpoints:")
    for backend in backends:
        print(f"  http://localhost:{backend.port}/ - {backend.name}")
        print(f"  http://localhost:{backend.port}/health - Health check")
        print(f"  http://localhost:{backend.port}/metrics - Metrics")
    print("\nPress Ctrl+C to stop all servers...")
    
    # Graceful shutdown handler
    def signal_handler(signum, frame):
        print(f"\nReceived signal {signum}, shutting down all backends...")
        loop = asyncio.get_event_loop()
        loop.create_task(shutdown_all(runners))
    
    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)
    
    # Keep running
    try:
        while True:
            await asyncio.sleep(1)
    except KeyboardInterrupt:
        print("\nKeyboard interrupt received, shutting down all backends...")
    finally:
        await shutdown_all(runners)

async def shutdown_all(runners):
    """Shutdown all backend runners"""
    for runner in runners:
        try:
            await runner.cleanup()
        except Exception as e:
            print(f"Error during cleanup: {e}")
    sys.exit(0)

def main():
    parser = argparse.ArgumentParser(description='Test backend server for Deflux load balancer')
    parser.add_argument('ports', nargs='+', type=int, help='Port(s) to run backend servers on')
    parser.add_argument('--name', default='test-backend', help='Backend name prefix')
    parser.add_argument('--delay', type=int, default=0, help='Response delay in milliseconds')
    parser.add_argument('--error-rate', type=float, default=0.0, help='Error rate (0.0-1.0)')
    parser.add_argument('--unhealthy', action='store_true', help='Start in unhealthy state')
    
    args = parser.parse_args()
    
    if not all(1024 < port < 65536 for port in args.ports):
        print("Error: All ports must be between 1024 and 65535")
        sys.exit(1)
    
    if not 0.0 <= args.error_rate <= 1.0:
        print("Error: Error rate must be between 0.0 and 1.0")
        sys.exit(1)
    
    print(f"Starting test backend server(s) on port(s): {args.ports}")
    print(f"Configuration: delay={args.delay}ms, error_rate={args.error_rate:.2%}, healthy={not args.unhealthy}")
    
    try:
        if len(args.ports) == 1:
            asyncio.run(run_backend(
                port=args.ports[0],
                name=args.name,
                delay_ms=args.delay,
                error_rate=args.error_rate,
                unhealthy=args.unhealthy
            ))
        else:
            asyncio.run(run_multiple_backends(
                ports=args.ports,
                base_name=args.name,
                delay_ms=args.delay,
                error_rate=args.error_rate,
                unhealthy=args.unhealthy
            ))
    except Exception as e:
        print(f"Error starting backend server(s): {e}")
        sys.exit(1)

if __name__ == '__main__':
    main()