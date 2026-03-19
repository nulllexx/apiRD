# Use Node 22
FROM node:22

# Set working directory
WORKDIR /home/useradmin/api/mainapi

# Copy package files first (for caching)
COPY package*.json ./

# Install node modules
RUN npm install

# Copy the rest of the app
COPY . .

# Expose your API port
EXPOSE 5000

# Run the app
CMD ["node", "src/app.js"]

