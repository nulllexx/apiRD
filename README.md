# Auth API

This project is a Node.js application that provides an API for user authentication using MongoDB with hashed passwords. It includes endpoints for user registration and login, ensuring secure handling of user credentials.

## Features

- User registration with password hashing
- User login with token generation
- Middleware for token verification
- MongoDB integration for user data storage

## Technologies Used

- Node.js
- Express
- MongoDB (Mongoose)
- Bcrypt for password hashing
- JSON Web Tokens (JWT)
- dotenv for environment variable management

## Setup Instructions

1. Clone the repository:
   ```
   git clone <repository-url>
   ```

2. Navigate to the project directory:
   ```
   cd auth-api
   ```

3. Install the dependencies:
   ```
   npm install
   ```

4. Create a `.env` file in the root directory and add your MongoDB connection string and JWT secret:
   ```
   MONGODB_URI=<your-mongodb-uri>
   JWT_SECRET=<your-jwt-secret>
   ```

5. Start the application:
   ```
   npm start
   ```

## API Endpoints

### Register

- **POST** `/api/auth/register`
- Request body: `{ "username": "user", "password": "pass" }`
- Response: `{ "token": "jwt-token" }`

### Login

- **POST** `/api/auth/login`
- Request body: `{ "username": "user", "password": "pass" }`
- Response: `{ "token": "jwt-token" }`

## License

This project is licensed under the MIT License.